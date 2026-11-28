#!/bin/sh
# Run by startplasma when the Plasma session ends: take back what the env
# script gave the user manager, so a GNOME login that reuses it starts clean.
systemctl --user unset-environment GTK_USE_PORTAL
