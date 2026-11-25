"""A GTK 3 window of ordinary widgets, for the look of a GTK 3 application.

Run inside a desktop session by `graphical::`: prints what the toolkit resolved,
then stays open until the time in argv[1] (seconds) has passed.
"""
import sys

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk

settings = Gtk.Settings.get_default()
print(
    "gtk3 theme=%s icons=%s dark=%s"
    % (
        settings.props.gtk_theme_name,
        settings.props.gtk_icon_theme_name,
        settings.props.gtk_application_prefer_dark_theme,
    ),
    flush=True,
)
win = Gtk.Window(title="GTK 3 probe")
win.set_default_size(560, 380)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, margin=18)
box.add(Gtk.Label(label="GTK 3: theme %s" % settings.props.gtk_theme_name))
row = Gtk.Box(spacing=8)
for text, css in (("Button", None), ("Suggested", "suggested-action"), ("Destructive", "destructive-action")):
    button = Gtk.Button(label=text)
    if css:
        button.get_style_context().add_class(css)
    row.add(button)
box.add(row)
box.add(Gtk.Entry(text="Text entry"))
box.add(Gtk.CheckButton(label="Check button", active=True))
box.add(Gtk.Switch(active=True, halign=Gtk.Align.START))
box.add(Gtk.ProgressBar(fraction=0.6))
box.add(Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1))
combo = Gtk.ComboBoxText()
for item in ("Combo box", "Second"):
    combo.append_text(item)
combo.set_active(0)
box.add(combo)
win.add(box)
win.connect("destroy", Gtk.main_quit)
win.show_all()
GLib.timeout_add_seconds(int(sys.argv[1]) if len(sys.argv) > 1 else 60, Gtk.main_quit)
Gtk.main()
