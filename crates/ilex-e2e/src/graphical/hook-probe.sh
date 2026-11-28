#!/bin/sh
# Source Plasma's login-time hook the way startplasma does, as login $1 of this
# user, in a shell of its own. With `loginctl-fails` as $2 the `loginctl` it finds
# fails. Prints its exit status, what it wrote to stdout (startplasma reads it)
# and what it wrote to stderr.
export XDG_SESSION_ID=$1
work=$(mktemp -d)
if [ "${2:-}" = loginctl-fails ]; then
    printf '#!/bin/sh\nexit 1\n' >"$work/loginctl"
    chmod +x "$work/loginctl"
    PATH=$work:$PATH
    export PATH
fi
out=$(. /etc/xdg/plasma-workspace/env/10-ilex-kde-stale.sh 2>"$work/err")
status=$?
echo "status=$status stdout=[$out]"
sed 's/^/stderr: /' "$work/err"
rm -rf "$work"
