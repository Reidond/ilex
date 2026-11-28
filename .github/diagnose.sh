# shellcheck shell=bash
# Sourced by the security CI scripts after `set -euo pipefail`. A command that
# fails under `set -e` is named in the job log with its status and location,
# followed by the tail of the logs the script registered with `diag_logs`.
# Without this a failing script ends silently while its own output is only in
# a log file that is not part of the job log.
#
#   source "$(dirname "${BASH_SOURCE[0]}")/diagnose.sh"
#   diag_logs "$dir/command.log" "$dir/security/result.json"   # replaces the list
#   diag_fail 'the nested run never reached its injected failure'  # report + exit 1
#
# Commands in an `if`/`&&`/`||`/`!` condition do not trigger the report, so an
# expected failure stays quiet. A failure inside a child script is reported by
# that child, which knows the logs; the parent names only its own command.
set -E
DIAG_NAME=$(basename "${BASH_SOURCE[1]:-$0}")
DIAG_TAIL=${DIAG_TAIL:-100}
DIAG_LOGS=()

# Replace the registered logs. An entry is a file or a glob; a glob shows its
# newest match, since tools create a fresh `run.XXXXXX` directory per attempt.
diag_logs() { DIAG_LOGS=("$@"); }

diag_show() {
    local pattern=$1 file
    # shellcheck disable=SC2012 # newest-first selection of a glob's matches
    file=$(ls -t -- $pattern 2>/dev/null | head -n 1) || true
    if [[ -z "$file" || ! -f "$file" ]]; then
        echo "(no $pattern)"
        return
    fi
    echo "::group::${DIAG_NAME}: last ${DIAG_TAIL} lines of ${file}"
    # Terminal colour sequences make the tail unreadable in the job log.
    { tail -n "$DIAG_TAIL" -- "$file" | sed -E 's/\x1b\[[0-9;]*[A-Za-z]//g'; } || true
    echo '::endgroup::'
}

# Print a failure and the registered log tails. It must not change the caller's
# shell options: after the ERR trap returns, errexit still ends the script.
diag_report() {
    trap - ERR
    echo "::error::${DIAG_NAME}: $1" >&2
    local pattern
    for pattern in ${DIAG_LOGS[@]+"${DIAG_LOGS[@]}"}; do
        diag_show "$pattern" >&2 || true
    done
    return 0
}

# ERR trap: expanded at the failing command, so $BASH_COMMAND and $LINENO are its.
diag_failure() {
    diag_report "\`$2\` failed with exit status $1 at $3"
}
trap 'diag_failure "$?" "$BASH_COMMAND" "${BASH_SOURCE[0]}:$LINENO"' ERR

diag_fail() {
    diag_report "$*"
    exit 1
}
