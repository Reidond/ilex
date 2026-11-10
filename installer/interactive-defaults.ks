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
# The installer Containerfile fills in both placeholders.
bootc --source-imgref @SOURCE_IMGREF@ --target-imgref @TARGET_IMGREF@

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
