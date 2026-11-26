#!/bin/bash
# The KDE shell's GTK look, offline, inside the booted image. Nothing it ships
# may reach a GNOME session of the same home: it writes no GTK file and no
# shared GSettings key, and what it does set is only reachable from Plasma.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }

# kde-gtk-config is what writes settings.ini, gtk-4.0, gtkrc-2.0 and GSettings
# into the home; the shell leaves it out, so nothing can.
for path in /usr/lib64/qt6/plugins/kf6/kded/gtkconfig.so \
    /usr/lib64/gtk-3.0/modules/libcolorreload-gtk-module.so \
    /usr/lib64/gtk-3.0/modules/libwindow-decorations-gtk-module.so; do
    [ ! -e "$path" ] || fail "$path ships"
done
for theme in Breeze Breeze-Dark; do
    for gtk in gtk-3.0 gtk-4.0; do
        [ -f "/usr/share/themes/$theme/$gtk/gtk.css" ] || fail "no $theme for $gtk"
    done
done
grep -qx 'UseIn=KDE' /usr/share/xdg-desktop-portal/portals/kde.portal
grep -qx 'org.freedesktop.impl.portal.Settings=kde;gtk;' /usr/share/xdg-desktop-portal/kde-portals.conf

# The Plasma env script sets the one variable GTK needs and nothing else; the
# shutdown script takes it back.
[ "$(grep -v '^#' /etc/xdg/plasma-workspace/env/50-ilex-kde.sh | grep .)" = 'export GTK_USE_PORTAL=1' ] ||
    fail "the env script exports more than GTK_USE_PORTAL"
grep -qx 'systemctl --user unset-environment GTK_USE_PORTAL' /etc/xdg/plasma-workspace/shutdown/50-ilex-kde.sh
test -x /etc/xdg/plasma-workspace/shutdown/50-ilex-kde.sh
[ "$(cat /usr/share/dconf/profile/ilex-kde)" = user-db:kde ]
for script in /usr/libexec/ilex/kde-gtk-sync /usr/libexec/ilex/kde-flathub /usr/libexec/ilex/portal-gtk; do
    test -x "$script"
    bash -n "$script"
done
# Only the Plasma session starts the sync and the Flathub remote.
for unit in ilex-kde-gtk ilex-kde-flathub; do
    test "$(readlink "/usr/lib/systemd/user/plasma-workspace.target.wants/$unit.service")" = "../$unit.service"
done
units=$(find /usr/lib/systemd /etc/systemd -name 'ilex-kde-*' 2>/dev/null | sort | tr '\n' ' ')
[ "$units" = '/usr/lib/systemd/user/ilex-kde-flathub.service /usr/lib/systemd/user/ilex-kde-gtk.service /usr/lib/systemd/user/plasma-workspace.target.wants/ilex-kde-flathub.service /usr/lib/systemd/user/plasma-workspace.target.wants/ilex-kde-gtk.service ' ] ||
    fail "unexpected KDE units or links: $units"
systemd-analyze --user verify --man=no /usr/lib/systemd/user/ilex-kde-gtk.service /usr/lib/systemd/user/ilex-kde-flathub.service
grep -qx 'ExecStart=/usr/libexec/ilex/portal-gtk' /usr/lib/systemd/user/xdg-desktop-portal-gtk.service.d/50-ilex-kde.conf

# The GTK settings portal reads the KDE-only database only while Plasma runs,
# decided when it starts from the desktop and the Plasma target of that moment.
work=$(mktemp -d)
sed 's|^exec /usr/libexec/xdg-desktop-portal-gtk|exec /usr/bin/env|' /usr/libexec/ilex/portal-gtk >"$work/portal-gtk"
printf '#!/bin/sh\nprintf "%%s\\n" "$STUB_STATE"\n' >"$work/systemctl"
chmod +x "$work/systemctl"
profile() { env -i PATH="$work:/usr/bin" "$@" sh "$work/portal-gtk" | grep '^DCONF_PROFILE=' || true; }
[ "$(profile XDG_CURRENT_DESKTOP=KDE STUB_STATE=active)" = DCONF_PROFILE=ilex-kde ] || fail "Plasma running: no KDE database"
[ "$(profile XDG_CURRENT_DESKTOP=KDE STUB_STATE=activating)" = DCONF_PROFILE=ilex-kde ] || fail "Plasma starting: no KDE database"
[ -z "$(profile XDG_CURRENT_DESKTOP=KDE STUB_STATE=inactive)" ] || fail "no Plasma, KDE desktop left over"
[ -z "$(profile XDG_CURRENT_DESKTOP=GNOME STUB_STATE=active)" ] || fail "GNOME session, Plasma target left over"
[ -z "$(profile XDG_CURRENT_DESKTOP=GNOME:GNOME-Classic STUB_STATE=active)" ] || fail "GNOME Classic"
[ -z "$(profile STUB_STATE=active)" ] || fail "no desktop"
echo "ok: the portal chooses the KDE database per start"

# The sync, in a real user session: dark and light, a custom icon and cursor
# theme and fonts, hostile values, and nothing in a shared place.
user=ilexgtk
id "$user" >/dev/null 2>&1 || useradd --create-home "$user"
uid=$(id -u "$user")
loginctl enable-linger "$user"
systemctl start "user@$uid.service"
for _ in $(seq 60); do [ -S "/run/user/$uid/bus" ] && break; sleep 1; done
home=$(getent passwd "$user" | cut -d: -f6)
as_user() {
    runuser -u "$user" -- env HOME="$home" XDG_RUNTIME_DIR="/run/user/$uid" \
        DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" "$@"
}
interface() { as_user env DCONF_PROFILE=ilex-kde dconf dump /org/gnome/desktop/interface/; }
expect() { # name expected-text
    [ "$(interface)" = "$2" ] || fail "$1: got
$(interface)"
}
write_config() { # kdeglobals-text kcminputrc-text
    mkdir -p "$home/.config"
    printf '%s' "$1" >"$home/.config/kdeglobals"
    printf '%s' "$2" >"$home/.config/kcminputrc"
    chown -R "$user:" "$home/.config"
}

write_config "[Colors:Window]
BackgroundNormal=32,35,38
[Icons]
Theme=breeze-dark
[General]
font=Noto Sans,11,-1,5,400,0,0,0,0,0,0,0,0,0,0,1
fixed=Hack,9,-1,5,400,0,0,0,0,0,0,0,0,0,0,1
" "[Mouse]
cursorTheme=Breeze_Snow
cursorSize=32
"
as_user /usr/libexec/ilex/kde-gtk-sync
expect "Breeze Dark" "[/]
color-scheme='prefer-dark'
cursor-size=32
cursor-theme='Breeze_Snow'
document-font-name='Noto Sans 11'
font-name='Noto Sans 11'
gtk-theme='Breeze-Dark'
icon-theme='breeze-dark'
monospace-font-name='Hack 9'"

write_config "" ""
as_user /usr/libexec/ilex/kde-gtk-sync
expect "defaults (Breeze Light)" "[/]
color-scheme='prefer-light'
cursor-size=24
cursor-theme='breeze_cursors'
document-font-name='Noto Sans 10'
font-name='Noto Sans 10'
gtk-theme='Breeze'
icon-theme='breeze'
monospace-font-name='Noto Sans Mono 10'"

# Values that are not names are never written into a GVariant.
write_config "[Colors:Window]
BackgroundNormal=1;2;x
[Icons]
Theme=x';y
[General]
font=Evil'Font,9,-1
fixed=a\$b,ten
" "[Mouse]
cursorTheme=a'b
cursorSize=-1
"
as_user /usr/libexec/ilex/kde-gtk-sync
expect "hostile values" "[/]
color-scheme='prefer-light'
cursor-size=24
cursor-theme='breeze_cursors'
document-font-name='Noto Sans 10'
font-name='Noto Sans 10'
gtk-theme='Breeze'
icon-theme='breeze'
monospace-font-name='Noto Sans Mono 10'"

# Nothing a desktop of the same home reads: no interface key in the user's own
# database (other keys, such as the login screen's, are not Plasma's), no GTK
# settings file.
[ -z "$(as_user dconf dump /org/gnome/desktop/interface/)" ] || fail "the user database holds interface keys:
$(as_user dconf dump /org/gnome/desktop/interface/)"
for path in .config/gtk-3.0 .config/gtk-4.0 .gtkrc-2.0 .config/xsettingsd; do
    [ ! -e "$home/$path" ] || fail "$home/$path exists"
done
echo "ok: the sync keeps Plasma's look in the KDE database only"
loginctl disable-linger "$user"
systemctl stop "user@$uid.service" || true
