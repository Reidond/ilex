#!/bin/bash
# SSH forced-command and sole sudo entry point. A finite protocol, never eval.
set -euo pipefail
if [ "$(id -u)" != 0 ]; then exec sudo /usr/local/libexec/ilex-builder-control; fi
# sudo deliberately preserves this one bounded operation value via an argument-free
# stdin frame, not arbitrary environment or host shell strings.
IFS= read -r op
case "$op" in probe|idle|smoke|update) ;; *) echo 'unsupported builder operation' >&2; exit 64;; esac
exec 8>/run/lock/ilex-builder-jobs.lock
flock -n 8 || { echo 'builder has active jobs' >&2; exit 75; }
if systemctl list-units 'ilex-job-*.service' --state=running --no-legend | grep -q .; then echo 'builder has active jobs' >&2; exit 75; fi
case "$op" in
  probe) cat /var/lib/ilex-builder/provision.json;;
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
          dnf --setopt=max_parallel_downloads=10 upgrade -y
          /bin/bash /var/lib/ilex-builder/provision.sh "$(cat /var/lib/ilex-builder/revision)";;
esac
