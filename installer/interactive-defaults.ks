# Kickstart defaults for the ilex installer ISO. Anaconda still asks for the
# disk, the user, the language and so on; this only fixes what is installed.
#
# --source-imgref: the ilex image embedded in the ISO's container storage.
#   `cargo xtask iso <image@digest>` embeds exactly that digest, under a local
#   tag, so a later push of :latest can't change what this ISO installs.
# --target-imgref: where the installed machine pulls updates from, so no
#   `bootc switch` is needed after an ISO install.
#
# The installer Containerfile fills in both placeholders.
bootc --source-imgref containers-storage:@PAYLOAD_REF@ --target-imgref @TARGET_IMGREF@
