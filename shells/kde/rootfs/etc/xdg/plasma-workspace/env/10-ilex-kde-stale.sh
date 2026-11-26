# Sourced by startplasma at the start of a Plasma session, before it starts
# anything, never by GNOME. POSIX sh, and quiet on stdout: startplasma reads the
# environment back from the shell that sources it.
#
# A Plasma that a crashed session left running in a user manager that outlives
# its sessions (lingering, SSH) is not this session's: a login that starts over
# it gets a compositor with no seat and a shell that shows nothing, with the
# greeter still on screen. kde-session-watch stops such a stack within seconds of
# the crash; a login that comes sooner stops it here, so it starts its own. Only
# a stack whose login session logind has dropped (or is closing) goes: if the
# session that started it is alive, this is a second session of the same user,
# which is not this script's to touch.
ilex_stale_plasma() {
    case $(systemctl --user show -P ActiveState plasma-workspace.target 2>/dev/null) in
    active | activating) ;;
    *) return 0 ;;
    esac
    ilex_old=$(systemctl --user show-environment 2>/dev/null | sed -n 's/^XDG_SESSION_ID=//p' | head -n 1)
    case $ilex_old in
    '' | "${XDG_SESSION_ID:-}" | *[!0-9A-Za-z]*) return 0 ;;
    esac
    if [ -e "/run/systemd/sessions/$ilex_old" ] &&
        [ "$(timeout 10 loginctl show-session "$ilex_old" --property=State --value 2>/dev/null)" != closing ]; then
        return 0
    fi
    echo "ilex: Plasma of the ended session $ilex_old is still up, stopping it" >&2
    # Every unit is named so that `stop` waits for it: graphical-session.target's
    # own job ends before the units that are only PartOf it have stopped.
    timeout 60 systemctl --user stop graphical-session.target 'plasma-workspace*.target' \
        'plasma-*.service' 'ilex-kde-*.service' 'xdg-desktop-portal*.service' >/dev/null 2>&1
    # Starting Plasma's target over a stop that is still running is refused as
    # destructive ("Could not start systemd managed Plasma session"): wait for
    # the manager to have nothing left to stop.
    ilex_n=0
    while [ -n "$(systemctl --user list-jobs --no-legend 2>/dev/null)" ] && [ "$ilex_n" -lt 120 ]; do
        sleep 0.5
        ilex_n=$((ilex_n + 1))
    done
    return 0
}
ilex_stale_plasma
unset -f ilex_stale_plasma
unset ilex_old ilex_n
