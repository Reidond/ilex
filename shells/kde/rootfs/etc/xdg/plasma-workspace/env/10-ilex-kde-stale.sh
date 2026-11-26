# Sourced by startplasma at the start of a Plasma session, before it starts
# anything, never by GNOME. POSIX sh, and quiet on stdout: startplasma reads the
# environment back from the shell that sources it.
#
# A Plasma that a crashed session left running in a user manager that outlives
# its sessions (lingering, SSH) is not this session's: a login that starts over
# it gets a compositor with no seat and a shell that shows nothing, with the
# greeter still on screen. kde-session-watch stops such a stack within seconds of
# the crash; a login that comes sooner stops it here, so it starts its own.
#
# The manager has one XDG_SESSION_ID, the one of whichever login imported its
# environment last: a login that then failed (GNOME's, refusing to start next to
# a live Plasma) leaves an ended session there next to a Plasma that is alive.
# So the stack goes only when nothing else of this user's can be its owner, and
# a doubt means no stop (the watch's own rule): the session recorded in the
# manager is gone or closing, logind lists no other graphical session of this
# user that is not closing (a failing `loginctl` counts as one), and no
# gnome-session target is up.
#
# The same stop may already be running, the watch's (it does not wait for it),
# and Plasma's target cannot start over a stop job that is still running
# ("Could not start systemd managed Plasma session: TransactionIsDestructive"):
# the targets go inactive at once, the compositor, the shell and the portals
# only when their own stop jobs finish. So whenever such a stop job exists the
# login waits for it, whoever started it, and only for stop jobs of Plasma's own
# units: another job of the manager is none of its business.

# startplasma reads this script's stdout and drops its stderr: what it decides
# goes to the journal too.
ilex_say() {
    echo "ilex: $*" >&2
    printf '%s\n' "$*" | systemd-cat -t ilex-kde-stale 2>/dev/null
    return 0
}

# Stop jobs queued for the units of Plasma's stack: the targets, Plasma's units,
# the portals and the shell's own.
ilex_stopping() {
    systemctl --user list-jobs --no-legend --full 2>/dev/null |
        grep -Eq '^ *[0-9]+ +(graphical-session\.target|plasma-[^ ]*|ilex-kde-[^ ]*|xdg-desktop-portal[^ ]*) +stop( |$)'
}

# Wait, 60 seconds at most, until there are none.
ilex_settle() {
    ilex_n=0
    if ilex_stopping; then
        ilex_say "Plasma's units are still stopping, waiting for them"
    fi
    while [ "$ilex_n" -lt 120 ] && ilex_stopping; do
        sleep 0.5
        ilex_n=$((ilex_n + 1))
    done
    if [ "$ilex_n" -ge 120 ]; then
        ilex_say "Plasma's units are still stopping after 60 seconds, going on"
    fi
}

# A session logind lists and has not closed. A `loginctl` that fails or is slow
# leaves it alive: only positive evidence ends a session.
ilex_live() {
    [ -e "/run/systemd/sessions/$1" ] &&
        [ "$(timeout 10 loginctl show-session "$1" --property=State --value 2>/dev/null)" != closing ]
}

# The graphical (wayland, x11, mir) sessions of this user that logind has not
# closed, other than $1 (the one the manager recorded) and $2 (this login), by
# id. A session that cannot be queried counts, as does a user that cannot be.
ilex_others() {
    ilex_ids=$(timeout 10 loginctl show-user "$(id -u)" --property=Sessions --value 2>/dev/null) || {
        echo '?'
        return 0
    }
    for ilex_s in $ilex_ids; do
        case $ilex_s in
        "$1" | "$2" | '' | *[!0-9A-Za-z]*) continue ;;
        esac
        [ -e "/run/systemd/sessions/$ilex_s" ] || continue
        ilex_props=$(timeout 10 loginctl show-session "$ilex_s" --property=Type --property=State 2>/dev/null) || {
            echo "$ilex_s"
            continue
        }
        case $ilex_props in
        *State=closing*) ;;
        *Type=wayland* | *Type=x11* | *Type=mir*) echo "$ilex_s" ;;
        esac
    done
    return 0
}

ilex_stale_plasma() {
    case $(systemctl --user show -P ActiveState plasma-workspace.target 2>/dev/null) in
    active | activating) ;;
    *)
        # No Plasma is up; one may be going down.
        ilex_settle
        return 0
        ;;
    esac
    ilex_old=$(systemctl --user show-environment 2>/dev/null | sed -n 's/^XDG_SESSION_ID=//p' | head -n 1)
    case $ilex_old in
    '' | "${XDG_SESSION_ID:-}" | *[!0-9A-Za-z]*) return 0 ;;
    esac
    # The session that started it is alive: this is a second session of the same user.
    ilex_live "$ilex_old" && return 0
    # A GNOME session owns graphical-session.target (it, too, would be stopped).
    ilex_gnome=$(systemctl --user list-units --plain --no-legend --state=active,activating,deactivating \
        'gnome-session*.target' 2>/dev/null) || return 0
    [ -z "$ilex_gnome" ] || return 0
    # The recorded session is not the only one that can own the stack.
    ilex_other=$(ilex_others "$ilex_old" "${XDG_SESSION_ID:-}" | tr '\n' ' ')
    if [ -n "$ilex_other" ]; then
        ilex_say "session(s) ${ilex_other% } of this user are live, leaving the running Plasma alone"
        return 0
    fi
    ilex_say "Plasma of the ended session $ilex_old is still up, stopping it"
    # Every unit is named so that `stop` waits for it: graphical-session.target's
    # own job ends before the units that are only PartOf it have stopped.
    timeout 60 systemctl --user stop graphical-session.target 'plasma-workspace*.target' \
        'plasma-*.service' 'ilex-kde-*.service' 'xdg-desktop-portal*.service' >/dev/null 2>&1
    ilex_settle
    return 0
}
ilex_stale_plasma
unset -f ilex_stale_plasma ilex_others ilex_live ilex_settle ilex_stopping ilex_say
unset ilex_old ilex_n ilex_ids ilex_s ilex_props ilex_gnome ilex_other
