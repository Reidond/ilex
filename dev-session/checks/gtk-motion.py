#!/usr/bin/python3
"""Visible real Wayland GL workload and independent clipboard client for e2e.
Not copied into either production or development images.
"""
import ctypes
import gi
import json
import math
import sys
import threading
import time
gi.require_version('Gtk', '4.0')
from gi.repository import Gtk, Gdk, GLib

gl = ctypes.CDLL('libGL.so.1')
gl.glGetString.restype = ctypes.c_char_p
gl.glClearColor.argtypes = [ctypes.c_float] * 4
app = Gtk.Application(application_id='org.ilex.DevelopmentCheck')
started = time.monotonic()
frames = []
last_report = started
reported = False


def activate(application):
    window = Gtk.ApplicationWindow(application=application, title='ilex native WHPX acceptance')
    window.set_default_size(960, 650)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
    window.set_child(box)
    box.append(Gtk.Label(label='Actual ilex / virgl / Wayland GLArea'))
    entry = Gtk.Entry()
    entry.set_placeholder_text('Independent Wayland clipboard client')
    box.append(entry)
    area = Gtk.GLArea()
    area.set_vexpand(True)
    box.append(area)

    def render(area, context):
        global last_report, reported
        now = time.monotonic()
        frames.append(now)
        if not reported:
            print(json.dumps({'renderer': gl.glGetString(0x1F01).decode(), 'version': gl.glGetString(0x1F02).decode()}), flush=True)
            reported = True
        gl.glClearColor((math.sin(now * 2) + 1) / 2, .2, .4, 1.)
        gl.glClear(0x4000)
        if now - last_report >= 5:
            print(json.dumps({'frames': len(frames), 'elapsed_seconds': now - started, 'callbacks_per_second_last5': sum(t > now - 5 for t in frames) / 5}), flush=True)
            last_report = now
        return True
    area.connect('render', render)
    area.add_tick_callback(lambda widget, clock: (widget.queue_render() or True))
    window.present()
    clipboard = Gdk.Display.get_default().get_clipboard()

    def command(line):
        value = json.loads(line)
        if value['op'] == 'set':
            clipboard.set(value['text'])
            print('{"set":true}', flush=True)
        elif value['op'] == 'get':
            clipboard.read_text_async(None, lambda c, result: print(json.dumps({'text': c.read_text_finish(result)}), flush=True))
        elif value['op'] == 'quit':
            application.quit()
        return False
    def read():
        for line in sys.stdin:
            GLib.idle_add(command, line)
        GLib.idle_add(application.quit)
    threading.Thread(target=read, daemon=True).start()


app.connect('activate', activate)
app.run([])
