# Kickstart defaults for the ilex installer ISO. Anaconda still asks for the
# disk, the user, the language and so on; this only fixes what is installed.
#
# Anaconda reads this file only when there is no kickstart of its own (a
# kickstart that wants these defaults has to `%include` it).
#
# --source-imgref: the image to install, pulled from the registry (the
#   `registry:` transport is required here). The installer carries no image:
#   it always installs the newest one, and needs a network connection (Anaconda
#   brings up the first wired link by itself).
# --target-imgref: where the installed machine pulls updates from, so no
#   `bootc switch` is needed after an ISO install (no transport here).
#
# Kernel override, then a kickstart pre-answer, then DMI detection. The
# generated include contains only validated references from this ISO's table.
%pre --erroronfail
python3 -I -S /usr/libexec/ilex-installer/ilex-installer-choice apply
%end

%include /tmp/ilex-bootc.ks

# ilex is a GNOME desktop: boot the installed system to the login screen.
# Without this Anaconda picks multi-user.target, because it looks for a
# display manager in the RPM database of the root it installed into and a
# bootc deployment has none there.
xconfig --startxonboot

# Anaconda writes an /etc/fstab line for `/`, but an ostree system's root is
# an overlay the initramfs mounts. With that line systemd-remount-fs.service
# tries to remount it with the btrfs options and fails ("overlay: No changes
# allowed in reconfigure"), leaving the new system `degraded`.
%post --erroronfail
sed -i -E '\|^[^#[:space:]]+[[:space:]]+/[[:space:]]|d' /etc/fstab
%end

# bootc's Anaconda payload does not install Flatpaks. Preload the chosen
# composition's system apps using the installed root's binaries and state.
# A bootc %post chroot lacks /dev/null. Set up runtime binds from
# the installer in a private mount namespace, leaving no mounts behind.
# Failure is non-fatal: retain the diagnostic and let ilex-apps.timer retry.
%post --nochroot --log=/mnt/sysroot/var/log/anaconda/ilex-apps.log
runtime_mounts() {
    python3 -I -S - <<'PY'
import hashlib,json,os,pathlib,re
root=os.path.realpath('/mnt/sysroot')
wanted={root+'/'+name for name in ('dev','proc','sys','run')}
lines=pathlib.Path('/proc/self/mountinfo').read_text().splitlines()
if not lines:raise ValueError('empty mount table')
mounts=[]
for line in lines:
    left,separator,right=line.partition(' - ')
    fields,filesystem=left.split(),right.split()
    if not separator or len(fields)<6 or len(filesystem)<3:raise ValueError('invalid mountinfo row')
    int(fields[0]);int(fields[1])
    point=re.sub(r'\\([0-7]{3})',lambda match:chr(int(match[1],8)),fields[4])
    if point in wanted:mounts.append([*fields,*filesystem])
try:status=hashlib.sha256(pathlib.Path('/run/ilex/apps/status').read_bytes()).hexdigest()
except FileNotFoundError:status=None
print(json.dumps({'schema':1,'mounts':mounts,'installer_status':status},sort_keys=True))
PY
}
mkdir -p /mnt/sysroot/var/lib/ilex/apps
runtime_mounts > /mnt/sysroot/var/lib/ilex/apps/installer-mounts-before
before=$?
result=1
attempted=0
rm -f /mnt/sysroot/var/lib/ilex/apps/installer-pidns
if [ -x /mnt/sysroot/usr/bin/ilexctl ] && [ -x /mnt/sysroot/usr/bin/flatpak ]; then
    if [ "$before" -eq 0 ]; then
        # Killing the PID namespace's init also kills detached descendants.
        attempted=1
        timeout --kill-after=15s 1800 unshare --mount --pid --fork \
            --kill-child=KILL --propagation private \
            /usr/libexec/ilex-installer/preload-apps /mnt/sysroot
        result=$?
    fi
else
    echo "This payload has no system apps helper; no Flatpaks to preload."
    result=0
fi
runtime_mounts > /mnt/sysroot/var/lib/ilex/apps/installer-mounts-after
python3 -I -S - "$attempted" <<'PY'
import json,os,pathlib,sys,time
state=pathlib.Path('/mnt/sysroot/var/lib/ilex/apps')
before=json.loads((state/'installer-mounts-before').read_text())
after=json.loads((state/'installer-mounts-after').read_text())
assert before['schema']==after['schema']==1,'invalid mount snapshot schema'
assert before==after,'preload changed caller mounts or live installer status'
if sys.argv[1]=='0':
    print('Preload not attempted; caller mounts and live installer status unchanged.')
    raise SystemExit(0)
namespace=(state/'installer-pidns').read_text().strip()
assert namespace.startswith('pid:[') and namespace.endswith(']'),namespace
deadline=time.monotonic()+5
while True:
    remaining=[]
    for process in pathlib.Path('/proc').iterdir():
        if not process.name.isdecimal():continue
        try:
            if os.readlink(process/'ns/pid')==namespace:remaining.append(process.name)
        except (FileNotFoundError,ProcessLookupError):pass
    if not remaining:break
    if time.monotonic()>=deadline:raise RuntimeError(f'preload processes survived: {remaining}')
    time.sleep(0.05)
print('Preload namespace empty; caller mounts and live installer status unchanged.')
PY
cleanup=$?
printf '%s\n' "$cleanup" > /mnt/sysroot/var/lib/ilex/apps/installer-cleanup-result
if [ "$cleanup" -ne 0 ] && [ "$result" -eq 0 ]; then result=1; fi
printf '%s\n' "$result" > /mnt/sysroot/var/lib/ilex/apps/installer-result
if [ "$result" -ne 0 ]; then
    echo "System apps are incomplete; ilex-apps.timer will retry after boot (exit $result)."
fi
exit 0
%end

# End-of-install enrollment runs after setup.json, target configuration and
# the Flatpak preload's private namespace has exited. Enrollment sees the
# installer's own mounts, including the unlocked target root and its devices.
# Secrets never pass through this script or Anaconda's script log.
%post --nochroot --erroronfail
/usr/libexec/ilex-installer/complete-enrollment
%end
