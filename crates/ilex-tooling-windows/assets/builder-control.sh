#!/bin/bash
# SSH forced-command and sole sudo entry point. A finite protocol, never eval.
set -euo pipefail
if [ "$(id -u)" != 0 ]; then
    [ "$#" -eq 0 ] || exit 64
    if [ -n "${SSH_ORIGINAL_COMMAND:-}" ]; then
        if [[ "$SSH_ORIGINAL_COMMAND" =~ ^terminal\ ([0-9a-f-]{36})\ ([0-9a-f-]{36})$ ]]; then
            exec sudo /usr/local/libexec/ilex-builder-control terminal "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
        fi
        exit 64
    fi
    exec sudo /usr/local/libexec/ilex-builder-control
fi
if [ "$#" -ne 0 ]; then
    [ "$#" -eq 3 ] && [ "$1" = terminal ] && [ -t 0 ] && [ -t 1 ] || exit 64
    exec /usr/bin/python3 -I -S /usr/local/libexec/ilex-builder-jobs --terminal "$2" "$3"
fi
# sudo deliberately preserves this one bounded operation value via an argument-free
# stdin frame, not arbitrary environment or host shell strings.
IFS= read -r op
case "$op" in probe|idle|smoke|update|job) ;; *) echo 'unsupported builder operation' >&2; exit 64;; esac
if [ "$op" = job ]; then exec /usr/bin/python3 -I -S /usr/local/libexec/ilex-builder-jobs; fi
if [ "$op" = probe ]; then cat /var/lib/ilex-builder/provision.json; exit 0; fi
exec 8>/run/lock/ilex-builder-jobs.lock
busy() {
    if [ "$op" = idle ]; then printf '{"schema":1,"idle":false,"reason":"%s"}\n' "$1"; exit 0; fi
    echo "builder is not idle: $1" >&2; exit 75
}
flock -n 8 || busy lifecycle_busy
# Failed inventories abort under set -e; they never establish absence.
units=$(timeout 5s systemctl list-units 'ilex-job-*.service' --state=active,activating,deactivating,reloading --no-legend)
[ -z "$units" ] || busy active_jobs
reason=$(/usr/bin/python3 -I -S /usr/local/libexec/ilex-builder-jobs --maintenance)
[ -z "$reason" ] || busy "$reason"
case "$op" in
  idle) printf '{"schema":1,"idle":true}\n';;
  smoke) /usr/local/libexec/ilex-builder-smoke;;
  update) python3 -c '
import hashlib,json,os,sys
p=json.loads(sys.stdin.readline(256*1024))
assert p["schema"]==1
recipe="\n".join(p[x] for x in ["provision","control","smoke"])
assert hashlib.sha256(recipe.encode()).hexdigest()==p["revision"]
for key,name in [("provision","provision.sh"),("control","builder-control.sh"),("smoke","smoke.sh"),("revision","revision")]:
    path="/var/lib/ilex-builder/"+name
    with open(path+".partial","w") as f: f.write(p[key]); f.flush(); os.fsync(f.fileno())
    os.replace(path+".partial",path)
'
          dnf --setopt=gpgcheck=1 --setopt=max_parallel_downloads=10 upgrade -y
          /bin/bash /var/lib/ilex-builder/provision.sh "$(cat /var/lib/ilex-builder/revision)";;
esac
