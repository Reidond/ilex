"""A window of ordinary widgets in GTK 3, GTK 4 or libadwaita, and what the toolkit resolved.

Run inside a desktop session, or inside a Flatpak sandbox, by `graphical::`:
`gtk-probe.py <gtk3|gtk4|adw> <seconds> [window]`. It prints one line,
`gtkprobe <toolkit> key=value ...`, then (with `window`) stays open until the
time has passed. The colours are what the style actually resolved for a
window's background and text, not the name of a theme: a missing theme
leaves the name set and the colours at the toolkit's default.
"""
import sys

import gi

toolkit = sys.argv[1]
seconds = int(sys.argv[2]) if len(sys.argv) > 2 else 60
show = len(sys.argv) > 3 and sys.argv[3] == "window"


def hexcolour(rgba):
    return "#%02x%02x%02x" % tuple(round(c * 255) for c in (rgba.red, rgba.green, rgba.blue))


def lookup(context, *names):
    for name in names:
        found, rgba = context.lookup_color(name)
        if found:
            return hexcolour(rgba)
    return "none"


if toolkit == "gtk3":
    gi.require_version("Gtk", "3.0")
    from gi.repository import GLib, Gtk

    settings = Gtk.Settings.get_default()
    win = Gtk.Window(title="GTK 3 probe")
    context = win.get_style_context()
    bg = lookup(context, "theme_bg_color")
    fg = lookup(context, "theme_fg_color")
    print(
        "gtkprobe gtk3 theme=%s icons=%s cursor=%s dark=%s modules=%s bg=%s fg=%s"
        % (
            settings.props.gtk_theme_name,
            settings.props.gtk_icon_theme_name,
            settings.props.gtk_cursor_theme_name,
            settings.props.gtk_application_prefer_dark_theme,
            settings.props.gtk_modules or "none",
            bg,
            fg,
        ),
        flush=True,
    )
    if show:
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
        GLib.timeout_add_seconds(seconds, Gtk.main_quit)
        Gtk.main()
else:
    gi.require_version("Gtk", "4.0")
    from gi.repository import GLib, Gtk

    adw = toolkit == "adw"
    if adw:
        gi.require_version("Adw", "1")
        from gi.repository import Adw

        Adw.init()
    else:
        Gtk.init()
    settings = Gtk.Settings.get_default()
    win = Adw.Window() if adw else Gtk.Window()
    win.set_title("libadwaita probe" if adw else "GTK 4 probe")
    context = win.get_style_context()
    bg = lookup(context, "window_bg_color", "theme_bg_color")
    fg = lookup(context, "window_fg_color", "theme_fg_color")
    line = "gtkprobe %s theme=%s icons=%s cursor=%s dark=%s bg=%s fg=%s" % (
        toolkit,
        settings.props.gtk_theme_name,
        settings.props.gtk_icon_theme_name,
        settings.props.gtk_cursor_theme_name,
        settings.props.gtk_application_prefer_dark_theme,
        bg,
        fg,
    )
    if adw:
        manager = Adw.StyleManager.get_default()
        accent = manager.get_accent_color() if hasattr(manager, "get_accent_color") else None
        line += " scheme=%s adw_dark=%s accent=%s" % (
            manager.get_color_scheme().value_nick,
            manager.get_dark(),
            accent.value_nick if accent is not None else "none",
        )
    print(line, flush=True)
    if show:
        win.set_default_size(560, 380)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, margin_top=18, margin_bottom=18, margin_start=18, margin_end=18)
        box.append(Gtk.Label(label="%s: theme %s" % (win.get_title(), settings.props.gtk_theme_name)))
        row = Gtk.Box(spacing=8)
        for text, css in (("Button", None), ("Suggested", "suggested-action"), ("Destructive", "destructive-action")):
            button = Gtk.Button(label=text)
            if css:
                button.add_css_class(css)
            row.append(button)
        box.append(row)
        entry = Gtk.Entry()
        entry.set_text("Text entry")
        box.append(entry)
        check = Gtk.CheckButton(label="Check button")
        check.set_active(True)
        box.append(check)
        switch = Gtk.Switch(halign=Gtk.Align.START)
        switch.set_active(True)
        box.append(switch)
        progress = Gtk.ProgressBar()
        progress.set_fraction(0.6)
        box.append(progress)
        box.append(Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1))
        if adw:
            win.set_content(box)
        else:
            win.set_child(box)
        loop = GLib.MainLoop()
        win.connect("close-request", lambda *_: loop.quit() or False)
        win.present()
        GLib.timeout_add_seconds(seconds, loop.quit)
        loop.run()
