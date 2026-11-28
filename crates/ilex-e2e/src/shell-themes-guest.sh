#!/bin/bash
# The KDE shell's GTK look, offline, inside the booted image. Nothing it ships
# may reach a GNOME session of the same home: it writes no GTK file and no
# shared GSettings key, and what it does set is only reachable from Plasma.
set -euo pipefail
fail() { echo "FAIL: $*" >&2; exit 1; }

# This script runs in the tier's one VM next to the other tests, and it is heavy
# (every stand-in is an exec on the guest's virtiofs root): let the guest finish
# booting first (tuned-ppd starts late, and a start slowed past its timeout fails
# the tier's `system_running` and `no_failed_units`), and stay out of the others' way.
timeout 300 systemctl is-system-running --wait >/dev/null 2>&1 || true
renice -n 19 $$ >/dev/null 2>&1 || true

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
for script in /usr/libexec/ilex/kde-gtk-sync /usr/libexec/ilex/kde-flathub /usr/libexec/ilex/portal-gtk \
    /usr/libexec/ilex/kde-session-watch; do
    test -x "$script"
    bash -n "$script"
done
# Only the Plasma session starts the sync, the Flathub remote and the session watch.
for unit in ilex-kde-gtk ilex-kde-flathub ilex-kde-session; do
    test "$(readlink "/usr/lib/systemd/user/plasma-workspace.target.wants/$unit.service")" = "../$unit.service"
done
units=$(find /usr/lib/systemd /etc/systemd -name 'ilex-kde-*' 2>/dev/null | sort | tr '\n' ' ')
[ "$units" = '/usr/lib/systemd/user/ilex-kde-flathub.service /usr/lib/systemd/user/ilex-kde-gtk.service /usr/lib/systemd/user/ilex-kde-session.service /usr/lib/systemd/user/plasma-workspace.target.wants/ilex-kde-flathub.service /usr/lib/systemd/user/plasma-workspace.target.wants/ilex-kde-gtk.service /usr/lib/systemd/user/plasma-workspace.target.wants/ilex-kde-session.service ' ] ||
    fail "unexpected KDE units or links: $units"
systemd-analyze --user verify --man=no /usr/lib/systemd/user/ilex-kde-gtk.service /usr/lib/systemd/user/ilex-kde-flathub.service \
    /usr/lib/systemd/user/ilex-kde-session.service
grep -qx 'ExecStart=/usr/libexec/ilex/portal-gtk' /usr/lib/systemd/user/xdg-desktop-portal-gtk.service.d/50-ilex-kde.conf
# The portal stops whenever Plasma's target does, so none that reads the KDE
# database outlives the session in a user manager that stays (linger, SSH, a crash).
grep -qx 'PartOf=plasma-workspace.target' /usr/lib/systemd/user/xdg-desktop-portal-gtk.service.d/50-ilex-kde.conf

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

# The session watch decides from the manager's environment and logind, in a
# fixed order, and a doubt is no stop. With `systemctl`, `loginctl`, `sleep`
# (the watch's one wait) and logind's session files stood in for, a later login
# of the same user (Plasma's, GNOME's, or one the environment does not name yet)
# that arrives before the watch acts must never cost it that session.
watch=$(mktemp -d)
mkdir "$watch/bin" "$watch/sessions" "$watch/state"
sed "s|/run/systemd/sessions|$watch/sessions|g" /usr/libexec/ilex/kde-session-watch >"$watch/script"
cat >"$watch/lib.sh" <<LIB
S=$watch/state
sessions=$watch/sessions
script=$watch/script
# session ID STATE TYPE: logind lists it.
session() { : >"\$sessions/\$1"; printf 'State=%s\nType=%s\n' "\$2" "\$3" >"\$S/session.\$1"; }
# drop ID: logind has removed it.
drop() { rm -f "\$sessions/\$1" "\$S/session.\$1"; }
# desktop ID DESKTOP: what the manager exports after that session's login.
desktop() { printf 'PATH=/usr/bin\nXDG_SESSION_ID=%s\nXDG_CURRENT_DESKTOP=%s\n' "\$1" "\$2" >"\$S/env"; }
LIB
cat >>"$watch/lib.sh" <<'LIB'
# endjob: the first stop job of one of Plasma's units has finished.
endjob() { sed -i '0,/^ *[0-9]\+ \+\(graphical-session\.target\|plasma-[^ ]*\|ilex-kde-[^ ]*\|xdg-desktop-portal[^ ]*\) \+stop/{//d}' "$S/jobs"; }
LIB
cat >"$watch/bin/systemctl" <<'STUB'
#!/bin/bash
. "$WATCH_LIB"
shift # --user
case $1 in
show-environment) cat "$S/env" ;;
is-active) [ -e "$S/plasma-up" ] ;;
list-units) cat "$S/gnome" ;;
show) cat "$S/target-state" ;;
list-jobs) [ ! -e "$S/jobs" ] || cat "$S/jobs" ;;
stop | --no-block) echo "$* [sessions: $(ls "$sessions" | tr '\n' ' ')]" >>"$S/calls" ;;
*) echo "stub systemctl: $*" >&2; exit 1 ;;
esac
STUB
# What the hook sends to the journal (systemd-cat reads it on stdin).
cat >"$watch/bin/systemd-cat" <<'STUB'
#!/bin/bash
. "$WATCH_LIB"
cat >>"$S/journal"
STUB
cat >"$watch/bin/loginctl" <<'STUB'
#!/bin/bash
. "$WATCH_LIB"
case $1 in
show-user) cat "$S/user-sessions" ;;
show-session)
    [ -e "$S/session.$2" ] || exit 1
    case " $* " in
    *" --value "*) sed -n 's/^State=//p' "$S/session.$2" ;;
    *) cat "$S/session.$2" ;;
    esac
    ;;
*) exit 1 ;;
esac
STUB
# Each wait of the watch runs the next line of the scenario, then really waits a
# moment. Once the scenario is over the watch is ended after 4 more waits
# (SIGTERM, status 143): a watch that decides nothing is counted, not timed, so
# a loaded host cannot cut a scenario short.
cat >"$watch/bin/sleep" <<'STUB'
#!/bin/bash
. "$WATCH_LIB"
echo $(($(cat "$S/sleeps" 2>/dev/null || echo 0) + 1)) >"$S/sleeps"
if [ -s "$S/events" ]; then
    line=$(head -n 1 "$S/events")
    sed -i 1d "$S/events"
    eval "$line"
else
    idle=$(($(cat "$S/idle" 2>/dev/null || echo 0) + 1))
    echo "$idle" >"$S/idle"
    # Only ever the watch itself (never another parent, init included).
    if [ "$idle" -ge 4 ] && grep -qaF "$script" "/proc/$PPID/cmdline" 2>/dev/null; then
        kill -TERM "$PPID"
    fi
fi
exec /usr/bin/sleep "$(cat "$S/real-sleep" 2>/dev/null || echo 0.2)"
STUB
chmod 755 "$watch"/bin/*

# scenario NAME EVENTS: session 5 is live and Plasma up under it; every wait of
# the watch then runs one line of EVENTS (an empty line is a wait in which
# nothing happens). A watch that decides nothing is ended by the stand-in
# `sleep` (status 143); the timeout is only a safety net.
scenario() {
    . "$watch/lib.sh"
    rm -rf "${sessions:?}"/* "${S:?}"/*
    session 5 active wayland
    desktop 5 KDE
    : >"$S/plasma-up" >"$S/user-sessions" >"$S/gnome" >"$S/calls"
    printf '%s' "${2:-}" >"$S/events"
    wstatus=0
    { nice -n 19 env PATH="$watch/bin:$PATH" WATCH_LIB="$watch/lib.sh" timeout 120 bash "$watch/script" 2>"$S/log" || wstatus=$?; } 2>/dev/null
    echo "     $1: status $wstatus; $(tr '\n' '|' <"$S/calls")"
}
# The calls record the sessions logind listed at the moment of the stop.
PLASMA='--no-block stop plasma-workspace*.target plasma-*.service xdg-desktop-portal.service [sessions: '
ALL='--no-block stop graphical-session.target [sessions: '
calls() { # wanted: the one stop, the watch ending of its own accord
    [ "$wstatus" -eq 0 ] && [ "$(cat "$S/calls")" = "$1" ] || fail "$name: status $wstatus, calls:
$(cat "$S/calls")
wanted: $1
log:
$(cat "$S/log")"
}
waits() { # the watch decided nothing: no stop, ended by the stand-in sleep
    [ "$wstatus" -eq 143 ] && [ ! -s "$S/calls" ] || fail "$name: status $wstatus, calls:
$(cat "$S/calls")
log:
$(cat "$S/log")"
}
said() { grep -q -- "$1" "$S/log" || fail "$name: no '$1' in the log:
$(cat "$S/log")"; }

name="a live session"
scenario "$name" ''
waits
name="a session that ended, nothing else of the user's"
scenario "$name" 'drop 5
'
calls "${ALL}]"
name="a session that is closing"
scenario "$name" 'session 5 closing wayland
'
calls "${ALL}5 ]"
name="a Plasma login before the watch acts"
scenario "$name" 'drop 5; session 6 active wayland; desktop 6 KDE
'
waits
said 'session 6 has taken over from 5'
name="... and that login ends, too"
scenario "$name" 'drop 5; session 6 active wayland; desktop 6 KDE

drop 6
'
calls "${ALL}]"
name="a GNOME login before the watch acts"
scenario "$name" 'drop 5; session 6 active wayland; desktop 6 GNOME
'
calls "${PLASMA}6 ]"
name="gnome-session's targets are up, the environment still says Plasma"
scenario "$name" 'drop 5; echo "gnome-session.target loaded active active GNOME" >"$S/gnome"
'
calls "${PLASMA}]"
name="the desktop changed, the session id did not"
scenario "$name" 'drop 5; desktop 5 GNOME:GNOME-Classic
'
calls "${PLASMA}]"
name="another graphical session is live, the environment says nothing yet"
scenario "$name" 'drop 5; session 6 online wayland; echo 6 >"$S/user-sessions"
'
waits
said 'session(s) 6 of this user are live: waiting'
name="... and it is GNOME's"
scenario "$name" 'drop 5; session 6 online wayland; echo 6 >"$S/user-sessions"


desktop 6 GNOME
'
calls "${PLASMA}6 ]"
name="... and it is Plasma's"
scenario "$name" 'drop 5; session 6 online wayland; echo 6 >"$S/user-sessions"


desktop 6 KDE
'
waits
said 'session 6 has taken over from 5'
name="... and it ends"
scenario "$name" 'drop 5; session 6 online wayland; echo 6 >"$S/user-sessions"

drop 6
'
calls "${ALL}]"
name="another session of the user is closing"
scenario "$name" 'drop 5; session 6 closing wayland; echo 6 >"$S/user-sessions"
'
calls "${ALL}6 ]"
name="another session of the user is a text login"
scenario "$name" 'drop 5; session 6 active tty; echo 6 >"$S/user-sessions"
'
calls "${ALL}6 ]"
name="another session of the user is X11"
scenario "$name" 'drop 5; session 6 active x11; echo 6 >"$S/user-sessions"
'
waits
name="logind cannot be asked about the other session"
scenario "$name" 'drop 5; : >"$sessions/6"; echo 6 >"$S/user-sessions"
'
waits
name="logind cannot list the user's sessions"
scenario "$name" 'drop 5; rm "$S/user-sessions"
'
waits
name="the manager's environment cannot be read"
scenario "$name" 'drop 5; : >"$S/env"


desktop 5 KDE
'
calls "${ALL}]"
name="Plasma was stopped by someone else meanwhile"
scenario "$name" 'drop 5; rm "$S/plasma-up"
'
[ "$wstatus" -eq 0 ] && [ ! -s "$S/calls" ] || fail "$name: status $wstatus, calls: $(cat "$S/calls")"
# Not watching at all: no session in the environment, or one logind does not know.
rm -rf "${sessions:?}"/* "${S:?}"/*
printf 'PATH=/usr/bin\nXDG_CURRENT_DESKTOP=KDE\n' >"$S/env"
status=0
env PATH="$watch/bin:$PATH" WATCH_LIB="$watch/lib.sh" bash "$watch/script" 2>/dev/null || status=$?
[ "$status" -eq 1 ] || fail "no session in the environment: status $status"
desktop 5 KDE
status=0
env PATH="$watch/bin:$PATH" WATCH_LIB="$watch/lib.sh" bash "$watch/script" 2>/dev/null || status=$?
[ "$status" -eq 1 ] || fail "a session logind does not know: status $status"

# A Plasma login starts from a clean manager: the env script stops what a crashed
# session left up (the compositor of a dead session has no seat, a session over
# it shows nothing), and only that. Session 6 is this login; the manager still
# exports session 5, whose Plasma may be up. The manager's one XDG_SESSION_ID is
# whichever login imported its environment last, so a session other than 5 can be
# the owner (a failed login that exported its id and closed): any other graphical
# session of the user that is not closing, or that cannot be asked about, is a
# reason to stop nothing, like a `loginctl` or `systemctl` that fails. A stop job
# of Plasma's units (the watch's own stop does not wait for them) is waited for
# whether or not Plasma's target is still up, and no other job is.
sed "s|/run/systemd/sessions|$watch/sessions|g" /etc/xdg/plasma-workspace/env/10-ilex-kde-stale.sh >"$watch/stale.sh"
stale() { # name wanted-calls wanted-waits setup...: the env script as startplasma sources it
    name=$1 wanted=$2 waits=$3
    . "$watch/lib.sh"
    rm -rf "${sessions:?}"/* "${S:?}"/*
    : >"$S/calls" >"$S/gnome"
    echo active >"$S/target-state"
    session 6 active wayland
    echo 6 >"$S/user-sessions"
    desktop 5 KDE
    shift 3
    for setup in "$@"; do eval "$setup"; done
    out=$(env PATH="$watch/bin:$PATH" WATCH_LIB="$watch/lib.sh" XDG_SESSION_ID=6 sh -c ". $watch/stale.sh" 2>"$S/log") ||
        fail "$name: the env script failed:
$(cat "$S/log")"
    [ -z "$out" ] || fail "$name: the env script wrote to stdout (startplasma reads it): $out"
    [ "$(tr '\n' '|' <"$S/calls")" = "$wanted" ] || fail "$name: calls were
$(tr '\n' '|' <"$S/calls")
wanted: $wanted
log:
$(cat "$S/log")"
    [ "$(cat "$S/sleeps" 2>/dev/null || echo 0)" = "$waits" ] || fail "$name: waited $(cat "$S/sleeps" 2>/dev/null || echo 0) times, wanted $waits
log:
$(cat "$S/log")"
}
# What the stand-in `systemctl list-jobs` lists: the columns are padded as the
# real ones. `drain N` ends N stop jobs of Plasma's units, one at each wait.
queue() { printf '%s\n' "$@" >"$S/jobs"; }
drain() { for _ in $(seq "$1"); do echo endjob >>"$S/events"; done; }
JOB_KWIN='   41 plasma-kwin_wayland.service    stop running'
JOB_PORTAL='   42 xdg-desktop-portal.service     stop waiting'
JOB_TARGET='   43 plasma-workspace.target        stop waiting'
JOB_SESSION='   44 graphical-session.target       stop running'
JOB_UNRELATED=('   50 plasma-kwin_wayland.service    start running'
    '   51 dbus-broker.service            stop running'
    '   52 xplasma-extra.service          stop waiting'
    '   53 app-org.kde.kate-4242.scope    stop running'
    '   54 xdg-document-portal.service    stop running')
STOP='stop graphical-session.target plasma-workspace*.target plasma-*.service ilex-kde-*.service xdg-desktop-portal*.service [sessions: 6 ]|'
STOP_7='stop graphical-session.target plasma-workspace*.target plasma-*.service ilex-kde-*.service xdg-desktop-portal*.service [sessions: 6 7 ]|'
stale "the crashed session's Plasma is up" "$STOP" 0
stale "... and is only starting" "$STOP" 0 'echo activating >"$S/target-state"'
stale "... and its session is closing" 'stop graphical-session.target plasma-workspace*.target plasma-*.service ilex-kde-*.service xdg-desktop-portal*.service [sessions: 5 6 ]|' 0 \
    'session 5 closing wayland' 'echo "5 6" >"$S/user-sessions"'
stale "no Plasma is up" "" 0 'echo inactive >"$S/target-state"'
stale "Plasma is going down" "" 0 'echo deactivating >"$S/target-state"'
stale "the session that started Plasma is alive" "" 0 'session 5 active wayland'
stale "the manager already exports this session" "" 0 'desktop 6 KDE'
stale "the manager exports no session" "" 0 'printf "PATH=/usr/bin\n" >"$S/env"'
# The manager names a session that is over, while another login of the user is
# alive: its Plasma is not a crashed session's.
stale "another Wayland session of the user is live, the manager's is gone" "" 0 \
    'session 7 active wayland' 'echo "6 7" >"$S/user-sessions"'
said 'session(s) 7 of this user are live'
grep -qx 'session(s) 7 of this user are live, leaving the running Plasma alone' "$S/journal" ||
    fail "$name: the decision did not reach the journal: $(cat "$S/journal" 2>&1)"
stale "... one that is only online" "" 0 'session 7 online wayland' 'echo "6 7" >"$S/user-sessions"'
stale "... an X11 one" "" 0 'session 7 active x11' 'echo "6 7" >"$S/user-sessions"'
stale "... the manager's session is closing" "" 0 'session 5 closing wayland' \
    'session 7 active wayland' 'echo "5 6 7" >"$S/user-sessions"'
stale "another session of the user cannot be asked about" "" 0 ': >"$sessions/7"' 'echo "6 7" >"$S/user-sessions"'
stale "logind cannot list the user's sessions" "" 0 'rm "$S/user-sessions"'
stale "another session of the user is closing" "$STOP_7" 0 'session 7 closing wayland' 'echo "6 7" >"$S/user-sessions"'
stale "another session of the user is a text login" "$STOP_7" 0 'session 7 active tty' 'echo "6 7" >"$S/user-sessions"'
stale "another session of the user has gone from logind meanwhile" "$STOP" 0 'echo "6 7" >"$S/user-sessions"'
stale "gnome-session's targets are up" "" 0 'echo "gnome-session.target loaded active active GNOME" >"$S/gnome"'
stale "the manager cannot list gnome-session's targets" "" 0 'rm "$S/gnome"'
# A stop that is still running: wait for it, with Plasma's target up or not.
stale "Plasma's target is down, the stop of its units still runs" "" 3 'echo inactive >"$S/target-state"' \
    'queue "$JOB_SESSION" "$JOB_KWIN" "$JOB_PORTAL"' 'drain 3'
said 'still stopping, waiting'
stale "... the target is deactivating" "" 1 'echo deactivating >"$S/target-state"' 'queue "$JOB_TARGET"' 'drain 1'
stale "... only the stop of the portal is left" "" 1 'echo inactive >"$S/target-state"' 'queue "$JOB_PORTAL"' 'drain 1'
stale "the leftovers are stopped, then the stop jobs they queued are waited for" "$STOP" 2 \
    'queue "$JOB_KWIN" "$JOB_TARGET"' 'drain 2'
# Only stop jobs of Plasma's units are waited for.
stale "other jobs only, Plasma's target is down" "" 0 'echo inactive >"$S/target-state"' 'queue "${JOB_UNRELATED[@]}"'
stale "other jobs only, Plasma is stale" "$STOP" 0 'queue "${JOB_UNRELATED[@]}"'
stale "other jobs and one stop job of Plasma's" "" 1 'echo inactive >"$S/target-state"' \
    'queue "${JOB_UNRELATED[0]}" "$JOB_KWIN" "${JOB_UNRELATED[@]:1}"' 'drain 1'
# A stop that never ends: bounded (120 waits of half a second), then login goes on.
stale "a stop job that never ends" "" 120 'echo inactive >"$S/target-state"' 'echo 0 >"$S/real-sleep"' 'queue "$JOB_KWIN"'
said 'still stopping after 60 seconds'
grep -qx "Plasma's units are still stopping after 60 seconds, going on" "$S/journal" ||
    fail "$name: the bound did not reach the journal: $(cat "$S/journal" 2>&1)"
# Nothing of the script is left in the shell that sourced it.
leak=$(env PATH="$watch/bin:$PATH" WATCH_LIB="$watch/lib.sh" XDG_SESSION_ID=6 sh -c ". $watch/stale.sh; set | grep -c '^ilex_' || true" 2>/dev/null)
[ "$leak" = 0 ] || fail "the env script leaves $leak variables or functions behind"
echo "ok: a Plasma login stops only what a crashed session left, and waits only for Plasma's own stop jobs"
rm -rf "$watch"
echo "ok: the session watch never stops a session that is live"

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
# The manager's merged unit: the stock PartOf and Plasma's.
parts=$(as_user systemctl --user show -P PartOf xdg-desktop-portal-gtk.service)
for target in graphical-session.target plasma-workspace.target; do
    case " $parts " in
    *" $target "*) ;;
    *) fail "xdg-desktop-portal-gtk.service is not PartOf $target (PartOf: $parts)" ;;
    esac
done
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

# Edge inputs a person or an older Plasma can leave in the files: zero-padded
# numbers (bash reads 08 as octal), spaces, an alpha channel, out-of-range or
# overflowing numbers, a wrong number of channels, and cursor sizes outside
# GTK's range. Each must sync (exit 0) to the look the values mean, or to the
# Breeze Light / 24 defaults when they are not a colour or a size.
edge() { # name BackgroundNormal cursorSize "color-scheme cursor-size gtk-theme"
    write_config "[Colors:Window]
BackgroundNormal=$2
" "[Mouse]
cursorSize=$3
"
    as_user /usr/libexec/ilex/kde-gtk-sync || fail "$1: the sync failed ($2 / $3)"
    got=$(interface | sed -n "s/^color-scheme='\(.*\)'\$/\1/p;s/^cursor-size=//p;s/^gtk-theme='\(.*\)'\$/\1/p" | tr '\n' ' ')
    [ "$got" = "$4 " ] || fail "$1: $2 / $3 gave '$got', expected '$4'"
}
edge "octal-looking channels" '08,09,08' 08 'prefer-dark 8 Breeze-Dark'
edge "padded channels" '009,010,011' 0 'prefer-dark 24 Breeze-Dark'
edge "spaces" '32, 35, 38' ' 64 ' 'prefer-dark 64 Breeze-Dark'
edge "padded size" '32,35,38' 024 'prefer-dark 24 Breeze-Dark'
edge "RGBA dark" '32,35,38,255' 32 'prefer-dark 32 Breeze-Dark'
edge "RGBA light, transparent" '255,255,255,0' 32 'prefer-light 32 Breeze'
edge "RGBA with spaces and padding" ' 009 , 010, 011 , 0255' 32 'prefer-dark 32 Breeze-Dark'
edge "size above the range" '239,240,241' 9999999999999999999999 'prefer-light 256 Breeze'
edge "size below the range" '239,240,241' 3 'prefer-light 8 Breeze'
edge "negative size" '239,240,241' -1 'prefer-light 24 Breeze'
edge "channel above 255" '300,0,0' 32 'prefer-light 32 Breeze'
edge "overflowing channel" '99999999999999999999,0,0' 32 'prefer-light 32 Breeze'
edge "negative channel" '-1,0,0' 32 'prefer-light 32 Breeze'
edge "two channels" '1,2' 32 'prefer-light 32 Breeze'
edge "five channels" '1,2,3,4,5' 32 'prefer-light 32 Breeze'
edge "empty channel" '32,,38' 32 'prefer-light 32 Breeze'
edge "bad alpha" '32,35,38,x' 32 'prefer-light 32 Breeze'

# The watcher stops when its D-Bus monitor does, with a failure, so that
# Restart=on-failure starts one that listens again (a loop that merely ended
# would exit 0 and systemd would leave the session without a watcher). A stub
# busctl first changes the look and announces it, then ends: the watcher
# syncs once at the start, once for the announcement, then fails.
stubs=$(mktemp -d)
chmod 755 "$stubs"
cat >"$stubs/busctl" <<'STUB'
#!/bin/sh
printf '[Colors:Window]\nBackgroundNormal=32,35,38\n' >"$HOME/.config/kdeglobals"
echo '{"type":"signal"}'
STUB
chmod 755 "$stubs/busctl"
write_config "" ""
status=0
as_user env PATH="$stubs:$PATH" /usr/libexec/ilex/kde-gtk-sync watch 2>"$stubs/stderr" || status=$?
[ "$status" -ne 0 ] || fail "the watcher exited 0 after its monitor ended"
grep -q 'monitor ended' "$stubs/stderr" || fail "the watcher gave no reason: $(cat "$stubs/stderr")"
expect "the announced change reached the database" "[/]
color-scheme='prefer-dark'
cursor-size=24
cursor-theme='breeze_cursors'
document-font-name='Noto Sans 10'
font-name='Noto Sans 10'
gtk-theme='Breeze-Dark'
icon-theme='breeze'
monospace-font-name='Noto Sans Mono 10'"
printf '#!/bin/sh\nexit 0\n' >"$stubs/busctl"
status=0
as_user env PATH="$stubs:$PATH" /usr/libexec/ilex/kde-gtk-sync watch 2>/dev/null || status=$?
[ "$status" -ne 0 ] || fail "the watcher exited 0 when its monitor never started"
rm -rf "$stubs"

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
