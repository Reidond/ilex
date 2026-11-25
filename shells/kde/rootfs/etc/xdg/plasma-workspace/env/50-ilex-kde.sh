# Sourced by startplasma at the start of a Plasma session, never by GNOME.
#
# kde-gtk-config, the GTK applications of a Plasma session and the GTK portal
# keep their GSettings (GTK and icon theme, cursor, fonts, colour scheme, scale)
# in a KDE dconf database of their own. Without this they write them to the
# user's shared "user" database, which would restyle a GNOME session of the
# same user. The profile is /usr/share/dconf/profile/ilex-kde.
export DCONF_PROFILE=ilex-kde
