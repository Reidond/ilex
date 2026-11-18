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
# composition's system apps in the installed root while its network is up.
# Failure is non-fatal: retain the diagnostic and let ilex-apps.timer retry.
%post --log=/var/log/anaconda/ilex-apps.log
if [ -x /usr/bin/ilexctl ] && [ -x /usr/bin/flatpak ]; then
    mkdir -p /var/lib/ilex/apps
    timeout 1800 /usr/bin/ilexctl apps sync
    result=$?
    printf '%s\n' "$result" > /var/lib/ilex/apps/installer-result
    if [ "$result" -ne 0 ]; then
        echo "System apps are incomplete; ilex-apps.timer will retry after boot (exit $result)."
    fi
else
    echo "This payload has no system apps helper; no Flatpaks to preload."
fi
exit 0
%end
