# Sourced by startplasma at the start of a Plasma session, never by GNOME.
#
# GTK applications of a Plasma session read their settings from the settings
# portal instead of GSettings (GTK_USE_PORTAL), so they get Plasma's look from
# xdg-desktop-portal-kde and, for the GTK theme, icons and fonts, from the
# KDE-only database that xdg-desktop-portal-gtk reads in Plasma. Flatpak
# applications always do. Left behind in a later GNOME session (a crash) this
# makes GTK applications ask GNOME's own portal, which answers from GNOME's own
# settings; the shutdown script next to this one removes it on a normal logout.
export GTK_USE_PORTAL=1
