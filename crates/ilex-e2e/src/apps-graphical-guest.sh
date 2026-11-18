# Runs as the authenticated homed user, on the real GNOME session bus.
set -euo pipefail
for entry in 'text/plain org.gnome.TextEditor.desktop' 'application/pdf org.gnome.Papers.desktop' 'image/png org.gnome.Loupe.desktop'; do
    read -r mime desktop <<< "$entry"
    gio mime "$mime" | sed -n '1p' | grep -F "$desktop"
done
test "$(xdg-settings get default-web-browser)" = org.mozilla.firefox.desktop
systemd-run --user --collect --unit=ilex-test-editor flatpak run org.gnome.TextEditor
systemd-run --user --collect --unit=ilex-test-firefox flatpak run org.mozilla.firefox --new-window about:blank
gio open "$HOME/ilex-apps.pdf"
for app in org.gnome.TextEditor org.mozilla.firefox org.gnome.Papers; do
    timeout 90 bash -c 'until flatpak ps --columns=application | grep -Fx "$1"; do sleep 2; done' sh "$app"
done

# Persist a real sandbox setting inside the encrypted home. Modern Flatpaks
# normally use a per-app keyfile; applications requesting dconf use the
# session's confined writer. Do not force a different backend for this test.
before=$(flatpak run --command=gsettings org.gnome.TextEditor get org.gnome.TextEditor show-line-numbers)
if [ "$before" = true ]; then desired=false; else desired=true; fi
flatpak run --command=gsettings org.gnome.TextEditor set org.gnome.TextEditor show-line-numbers "$desired"
test "$(flatpak run --command=gsettings org.gnome.TextEditor get org.gnome.TextEditor show-line-numbers)" = "$desired"
backend=$(flatpak run --command=sh org.gnome.TextEditor -c 'printf %s "${GSETTINGS_BACKEND:-default}"')
echo "Text Editor settings backend: $backend"
keyfile="$HOME/.var/app/org.gnome.TextEditor/config/glib-2.0/settings/keyfile"
if [ -f "$keyfile" ]; then
    grep -F "show-line-numbers=$desired" "$keyfile"
    test "$(stat -c %u "$keyfile")" = "$(id -u)"
else
    test "$(dconf read /org/gnome/TextEditor/show-line-numbers)" = "$desired"
fi

# GNOME 51 consumes the apps' integration D-Bus properties, keeping shell-owned
# settings. A real serialized location avoids depending on a weather service.
location=$(gjs -c "imports.gi.versions.GWeather='4.0'; print(imports.gi.GWeather.Location.get_world().find_by_station_code('UKBB').serialize().print(true));")
flatpak run --command=gsettings org.gnome.clocks set org.gnome.clocks world-clocks "[{'location': <$location>}]"
flatpak run --command=gsettings org.gnome.Weather set org.gnome.Weather locations "[<$location>]"
systemd-run --user --collect --unit=ilex-test-clocks flatpak run org.gnome.clocks
systemd-run --user --collect --unit=ilex-test-weather flatpak run org.gnome.Weather
timeout 90 bash -c 'until gsettings get org.gnome.shell.world-clocks locations | grep -F UKBB && gsettings get org.gnome.shell.weather locations | grep -F UKBB; do sleep 2; done'

# Use Software's public GAction (the same loader/transaction as Install), then
# assert the resulting installation independently of the UI presentation.
flatpak remote-add --user --if-not-exists flathub /usr/share/flatpak/remotes.d/flathub.flatpakrepo
flatpak update --user --appstream -y --noninteractive
systemd-run --user --collect --unit=ilex-test-software gnome-software --verbose --search Chess
timeout 180 bash -c 'until gdbus call --session --dest org.gnome.Software --object-path /org/gnome/Software --method org.gtk.Actions.Describe install >/dev/null; do sleep 2; done'
# Discover the source through Software itself, as Shell search does. Do not
# force user scope in an ambiguous ID: every offered Flathub result must have
# user scope, with the system catalogue hidden.
catalogue=$(busctl --user --json=short call org.gnome.Software /org/gnome/Software/SearchProvider org.gnome.Shell.SearchProvider2 GetInitialResultSet as 1 Chess)
echo "GNOME Software discovered: $catalogue"
data_id=$(printf '%s\n' "$catalogue" | python3 -c 'import json,sys; refs=[r for r in json.load(sys.stdin)["data"][0] if "/flatpak/flathub/" in r]; assert refs and all(r.startswith("user/") for r in refs),refs; matches=[r for r in refs if r.split("/")[1:]==["flatpak","flathub","org.gnome.Chess","stable"]]; assert len(matches)==1,matches; print(matches[0])')
gdbus call --session --dest org.gnome.Software --object-path /org/gnome/Software --method org.gtk.Actions.Activate install "[<('$data_id', uint32 0)>]" '{}'
timeout 300 bash -c 'until flatpak info --user org.gnome.Chess; do sleep 2; done'
! flatpak info --system org.gnome.Chess
# Reuse the system runtime for the per-user app.
! flatpak info --user org.gnome.Platform//51
