#!/usr/bin/python3 -I
"""Development-only session adapter. Bounded frames, no clipboard logging.

Root supervises the current unlocked local Wayland user. Clipboard access runs
as that user, never at GDM. The SSH EOF/lifetime owns the entire helper tree.
GNOME 51's private Mutter API is probed before use, not treated as stable ABI.
"""
import gi
import json
import os
import queue
import selectors
import signal
import stat
import struct
import subprocess
import sys
import threading
import time
import uuid
import hashlib
from pathlib import Path
from gi.repository import Gio, GLib

MAX_TEXT = 256 * 1024
MAX_FRAME = 2 * 1024 * 1024  # JSON escaping can expand a valid bounded UTF-8 string
HELPER = os.path.realpath(__file__)
output_lock = threading.Lock()
ending = threading.Event()


def emit(value):
    data = json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
    if len(data) > MAX_FRAME:
        raise ValueError('frame limit')
    with output_lock:
        sys.stdout.buffer.write(struct.pack('!I', len(data)) + data)
        sys.stdout.buffer.flush()


def frames(stream):
    descriptor = stream.fileno()
    def exact(count, deadline=None):
        data = bytearray()
        while len(data) < count:
            if ending.is_set():
                raise EOFError()
            if deadline is not None and time.monotonic() >= deadline:
                raise ValueError('frame deadline')
            with selectors.DefaultSelector() as poll:
                poll.register(descriptor, selectors.EVENT_READ)
                if not poll.select(.1):
                    continue
            part = os.read(descriptor, count - len(data))
            if not part:
                raise EOFError()
            data.extend(part)
        return bytes(data)
    while True:
        first = exact(1)
        deadline = time.monotonic() + 2
        size = struct.unpack('!I', first + exact(3, deadline))[0]
        if not 0 < size <= MAX_FRAME:
            raise ValueError('frame limit')
        value = json.loads(exact(size, deadline).decode('utf-8'))
        if not isinstance(value, dict):
            raise ValueError('invalid frame')
        if 'text' in value and (not isinstance(value['text'], str) or len(value['text'].encode('utf-8')) > MAX_TEXT or '\0' in value['text']):
            raise ValueError('text limit')
        yield value


system = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)


def dbus(connection, dest, path, interface, method, args=None, timeout=2000):
    return connection.call_sync(dest, path, interface, method, args, None,
                                Gio.DBusCallFlags.NONE, timeout, None)


def active_session():
    sessions = dbus(system, 'org.freedesktop.login1', '/org/freedesktop/login1',
                    'org.freedesktop.login1.Manager', 'ListSessions').unpack()[0]
    for sid, uid, user, seat, path in sessions:
        if uid < 1000 or not seat:
            continue
        props = dbus(system, 'org.freedesktop.login1', path, 'org.freedesktop.DBus.Properties',
                     'GetAll', GLib.Variant('(s)', ('org.freedesktop.login1.Session',))).unpack()[0]
        if props.get('Active') and props.get('Type') == 'wayland' and props.get('Class') == 'user' and not props.get('LockedHint'):
            return sid, uid, user
    return None


def drm_preferred_modeline(name, target):
    # Aquamarine can retain its original mode list after virtio EDID changes.
    # Use only the kernel-advertised preferred timing for this exact connector
    # and target, never a synthesized arbitrary mode or persistent config edit.
    for connector in Path('/sys/class/drm').glob('card*-*'):
        if connector.name.split('-', 1)[-1] != name:
            continue
        try:
            with (connector / 'modes').open() as stream:
                modes = stream.read(4096).splitlines()
            with (connector / 'edid').open('rb') as stream:
                edid = stream.read(128)
        except OSError:
            continue
        if (f'{target[0]}x{target[1]}' not in modes or len(edid) != 128 or
                edid[:8] != b'\x00\xff\xff\xff\xff\xff\xff\x00' or sum(edid) % 256):
            continue
        d = edid[54:72]
        width, height = d[2] | ((d[4] & 0xf0) << 4), d[5] | ((d[7] & 0xf0) << 4)
        hblank, vblank = d[3] | ((d[4] & 15) << 8), d[6] | ((d[7] & 15) << 8)
        hoff, hsync = d[8] | ((d[11] & 0xc0) << 2), d[9] | ((d[11] & 0x30) << 4)
        voff, vsync = (d[10] >> 4) | ((d[11] & 12) << 2), (d[10] & 15) | ((d[11] & 3) << 4)
        clock = d[0] | (d[1] << 8)
        if ((width, height) != target or not clock or d[17] & 0x80 or
                d[17] & 0x18 != 0x18 or not (0 < hoff < hoff + hsync <= hblank) or
                not (0 < voff < voff + vsync <= vblank)):
            continue
        return (f'modeline {clock / 100:.2f} {width} {width + hoff} '
                f'{width + hoff + hsync} {width + hblank} {height} {height + voff} '
                f'{height + voff + vsync} {height + vblank} '
                f'{"+" if d[17] & 2 else "-"}hsync {"+" if d[17] & 4 else "-"}vsync')
    return None


def display_info(target=None):
    # Read-only mode inspection is also useful at GDM; clipboard admission
    # remains restricted to unlocked user sessions above.
    if os.getuid() == 0:
        sessions = dbus(system, 'org.freedesktop.login1', '/org/freedesktop/login1',
                        'org.freedesktop.login1.Manager', 'ListSessions').unpack()[0]
        for sid, uid, user, seat, path in sessions:
            props = dbus(system, 'org.freedesktop.login1', path, 'org.freedesktop.DBus.Properties',
                         'GetAll', GLib.Variant('(s)', ('org.freedesktop.login1.Session',))).unpack()[0]
            if seat and props.get('Active') and props.get('Type') == 'wayland':
                return subprocess.run(['runuser', '-u', user, '--', 'env',
                    f'XDG_RUNTIME_DIR=/run/user/{uid}', f'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus',
                    'ILEX_DEV_DISPLAY_SESSION=' + json.dumps({'id': sid, 'uid': uid, 'leader': props.get('Leader'), 'started_monotonic': props.get('TimestampMonotonic')}),
                    HELPER] + (['--display-info'] if target is None else ['--resize', str(target[0]), str(target[1])]), check=True).returncode
        raise ValueError('no active display session')
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    try:
        state = dbus(bus, 'org.gnome.Mutter.DisplayConfig', '/org/gnome/Mutter/DisplayConfig',
                     'org.gnome.Mutter.DisplayConfig', 'GetCurrentState').unpack()
        monitors = []
        for identity, modes, properties in state[1]:
            current = [mode for mode in modes if mode[-1].get('is-current')]
            preferred = [mode for mode in modes if mode[-1].get('is-preferred')]
            monitors.append({'name': identity[0], 'current': [list(m[1:3]) for m in current],
                             'preferred': [list(m[1:3]) for m in preferred]})
        owner = dbus(bus, 'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus', 'GetNameOwner', GLib.Variant('(s)', ('org.gnome.Mutter.DisplayConfig',))).unpack()[0]
        reply = {'schema': 1, 'backend': 'mutter', 'monitors': monitors, 'display_instance': owner}
    except GLib.Error:
        environment = hypr_environment()
        result = subprocess.run(['hyprctl', '-j', 'monitors'], env=environment, capture_output=True, timeout=2, check=True)
        monitors = json.loads(result.stdout)
        if target is not None and len(monitors) == 1:
            monitor = monitors[0]
            prefix = f'{target[0]}x{target[1]}@'
            mode = next((m for m in monitor['availableModes'] if m.startswith(prefix)), None)
            if mode is None:
                mode = drm_preferred_modeline(monitor['name'], target)
            if mode and (monitor['width'], monitor['height']) != target:
                # Hyprland 0.55+ uses Lua, including runtime monitor changes.
                # Decimal byte escapes are valid Lua strings even for unusual
                # connector names, without allowing them to become Lua code.
                def lua_string(text):
                    return '"' + ''.join(f'\\{byte:03d}' for byte in text.encode('utf-8')) + '"'
                statement = ('hl.monitor({output=' + lua_string(monitor['name']) +
                    ',mode=' + lua_string(mode.removesuffix('Hz')) + ',position="auto",scale=1})')
                applied = subprocess.run(['hyprctl', 'eval', statement], env=environment,
                    capture_output=True, timeout=2)
                if applied.returncode != 0 or applied.stdout.strip() != b'ok':
                    subprocess.run(['hyprctl', 'keyword', 'monitor', f"{monitor['name']},{mode.removesuffix('Hz')},auto,1"], env=environment, capture_output=True, check=True, timeout=2)
                monitors = json.loads(subprocess.run(['hyprctl', '-j', 'monitors'], env=environment, capture_output=True, check=True, timeout=2).stdout)
        reply = {'schema': 1, 'backend': 'hyprland', 'display_instance': environment['HYPRLAND_INSTANCE_SIGNATURE'], 'monitors':
            [{'name': m['name'], 'current': [[m['width'], m['height']]], 'available': m['availableModes']} for m in monitors]}
    reply['session'] = json.loads(os.environ.get('ILEX_DEV_DISPLAY_SESSION', 'null'))
    reply['edid'] = []
    for connector in Path('/sys/class/drm').glob('card*-*'):
        try:
            with (connector / 'edid').open('rb') as stream:
                content = stream.read(32768)
            if content:
                reply['edid'].append({'connector': connector.name, 'sha256': hashlib.sha256(content).hexdigest()})
        except OSError:
            pass
    if target is not None:
        reply['target'] = list(target)
        reply['acknowledged'] = any(list(target) in m['current'] for m in reply['monitors'])
    print(json.dumps(reply), flush=True)


def hypr_environment():
    environment = dict(os.environ)
    # The compositor sets these variables after exec, so /proc/PID/environ
    # need not contain them. Use hyprctl's instance/Wayland-socket discovery,
    # and qualify its PID and socket against this admitted user's lifetime.
    result = subprocess.run(['hyprctl', '-j', 'instances'], env=environment,
                            capture_output=True, timeout=2, check=True)
    candidates = []
    runtime = f'/run/user/{os.getuid()}'
    for instance in json.loads(result.stdout):
        try:
            pid = int(instance['pid'])
            process = f'/proc/{pid}'
            if os.stat(process).st_uid != os.getuid() or open(process + '/comm').read().strip() != 'Hyprland':
                continue
            signature, socket = instance['instance'], instance['wl_socket']
            if not all(isinstance(value, str) and value and '/' not in value and value not in ['.', '..'] for value in [signature, socket]):
                continue
            node = os.stat(f'{runtime}/{socket}', follow_symlinks=False)
            if node.st_uid == os.getuid() and stat.S_ISSOCK(node.st_mode):
                candidates.append((signature, socket))
        except (OSError, ValueError, KeyError):
            continue
    if len(candidates) != 1:
        raise ValueError('no unambiguous live Hyprland session')
    environment['HYPRLAND_INSTANCE_SIGNATURE'], environment['WAYLAND_DISPLAY'] = candidates[0]
    return environment


def supervise():
    # No shell expansion or log of incoming data. A bounded queue breaks SSH
    # backpressure instead of growing memory while a session is unavailable.
    incoming = queue.Queue(8)
    def reader():
        try:
            for value in frames(sys.stdin.buffer):
                incoming.put(value, timeout=2)
        except (EOFError, ValueError, OSError, queue.Full):
            pass
        finally:
            ending.set()
    threading.Thread(target=reader, daemon=True).start()
    child = None
    relay_thread = None
    selected = None
    last_attempt = 0
    def stop_child():
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(2)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
    try:
        while not ending.is_set():
            current = active_session()
            if child is not None and (current != selected or child.poll() is not None):
                stop_child()
                if relay_thread is not None:
                    relay_thread.join(2)
                child = None
                emit({'op': 'inactive', 'schema': 1})
            if current is not None and child is None and time.monotonic() - last_attempt >= 1:
                selected = current
                sid, uid, user = current
                last_attempt = time.monotonic()
                child = subprocess.Popen(['runuser', '-u', user, '--', 'env',
                    f'XDG_RUNTIME_DIR=/run/user/{uid}', f'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus',
                    f'XDG_SESSION_ID={sid}', HELPER, '--session'],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    start_new_session=True)
                def relay(stream):
                    try:
                        for message in frames(stream):
                            emit(message)
                    except (EOFError, OSError, ValueError):
                        pass
                relay_thread = threading.Thread(target=relay, args=(child.stdout,), daemon=True)
                relay_thread.start()
            try:
                value = incoming.get(timeout=.2)
            except queue.Empty:
                continue
            if child is not None:
                # One bounded writer, killed on cancellation/lock. PIPE_BUF is
                # not assumed to fit a clipboard message.
                data = json.dumps(value, ensure_ascii=False).encode('utf-8')
                os.set_blocking(child.stdin.fileno(), False)
                payload = memoryview(struct.pack('!I', len(data)) + data)
                deadline = time.monotonic() + 2
                while payload:
                    if ending.is_set() or active_session() != selected or time.monotonic() > deadline:
                        child.terminate()
                        break
                    try:
                        written = os.write(child.stdin.fileno(), payload)
                        payload = payload[written:]
                    except BlockingIOError:
                        time.sleep(.01)
                    except BrokenPipeError:
                        break
    finally:
        if child is not None:
            stop_child()


def session():
    selected = active_session()
    if selected is None or selected[1] != os.getuid():
        return
    bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    generation = str(uuid.uuid4())
    loop = GLib.MainLoop()
    content = b''
    host_sequence = 0
    guest_sequence = 0
    transferring = False
    selection_epoch = 0
    selection_mime = None
    incoming = queue.Queue(8)
    outgoing = queue.Queue(8)
    enabled = True
    dest = 'org.gnome.Mutter.RemoteDesktop'
    interface = dest + '.Session'
    path = None
    watcher = None
    copy_owner = None

    def stop_helper(process):
        if process is None:
            return
        process.terminate()
        try:
            process.wait(1)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    def allowed():
        if not enabled or active_session() != selected:
            return False
        if path is not None:
            return not dbus(bus, 'org.gnome.ScreenSaver', '/org/gnome/ScreenSaver',
                            'org.gnome.ScreenSaver', 'GetActive').unpack()[0]
        return True

    def call(method, args=None):
        return dbus(bus, dest, path, interface, method, args)

    def fdcall(method, args):
        reply, fds = bus.call_with_unix_fd_list_sync(dest, path, interface, method, args,
                          None, Gio.DBusCallFlags.NONE, 2000, None, None)
        return fds.get(reply.unpack()[0])

    def pipe_io(fd, data=None):
        os.set_blocking(fd, False)
        deadline = time.monotonic() + 2
        result = bytearray()
        remaining = memoryview(data) if data is not None else None
        try:
            with selectors.DefaultSelector() as poll:
                poll.register(fd, selectors.EVENT_WRITE if data is not None else selectors.EVENT_READ)
                while time.monotonic() < deadline and not ending.is_set():
                    if not poll.select(.05):
                        continue
                    if data is None:
                        part = os.read(fd, min(65536, MAX_TEXT + 1 - len(result)))
                        if not part:
                            return bytes(result)
                        result.extend(part)
                        if len(result) > MAX_TEXT:
                            raise ValueError('selection exceeds limit')
                    else:
                        remaining = remaining[os.write(fd, remaining):]
                        if not remaining:
                            return True
            raise TimeoutError()
        finally:
            os.close(fd)

    def send_text(data):
        nonlocal guest_sequence, content
        if not allowed() or len(data) > MAX_TEXT:
            return
        text = data.decode('utf-8')
        if '\0' in text or data == content:
            return
        content = data
        guest_sequence += 1
        emit({'op': 'text', 'schema': 1, 'generation': generation,
              'origin': 'guest', 'sequence': guest_sequence, 'text': text})

    def read_selection():
        nonlocal transferring
        if transferring or selection_mime is None or not allowed():
            return
        transferring = True
        epoch = selection_epoch
        try:
            fd = fdcall('SelectionRead', GLib.Variant('(s)', (selection_mime,)))
        except GLib.Error:
            transferring = False
            return
        def worker():
            try:
                data = pipe_io(fd)
                GLib.idle_add(deliver, data)
            except (OSError, ValueError, TimeoutError):
                GLib.idle_add(deliver, None)
        def deliver(data):
            nonlocal transferring
            transferring = False
            if data is not None and epoch == selection_epoch:
                send_text(data)
            elif epoch != selection_epoch and selection_mime is not None:
                # A new offer arrived while the previous FD was in flight.
                # Discard the old bytes, then consume the latest owner.
                read_selection()
            return False
        threading.Thread(target=worker, daemon=True).start()

    def on_signal(conn, sender, obj, iface, name, args):
        nonlocal selection_epoch, selection_mime
        if name == 'Closed':
            loop.quit()
        elif name == 'SelectionOwnerChanged':
            selection_epoch += 1
            options = args.unpack()[0]
            types = options.get('mime-types', ())
            # Mutter 51 encodes this option as a (as) variant, which PyGObject
            # unpacks to a one-tuple containing the MIME list.
            if isinstance(types, tuple) and len(types) == 1 and isinstance(types[0], (list, tuple)):
                types = types[0]
            selection_mime = next((t for t in ['text/plain;charset=utf-8', 'text/plain'] if isinstance(types, (list, tuple)) and t in types), None)
            if options.get('session-is-owner'):
                selection_mime = None
            if selection_mime is not None:
                read_selection()
        elif name == 'SelectionTransfer':
            mime, serial = args.unpack()
            if not allowed():
                call('SelectionWriteDone', GLib.Variant('(ub)', (serial, False)))
                return
            fd = fdcall('SelectionWrite', GLib.Variant('(u)', (serial,)))
            data = content
            def write():
                try:
                    pipe_io(fd, data)
                    success = True
                except (OSError, TimeoutError):
                    success = False
                def done():
                    if allowed():
                        call('SelectionWriteDone', GLib.Variant('(ub)', (serial, success)))
                    return False
                GLib.idle_add(done)
            threading.Thread(target=write, daemon=True).start()

    # Detect the compositor from this user's live session, not a root-created
    # XWayland clipboard owner. The API's current GNOME major is intentional.
    try:
        version = dbus(bus, 'org.gnome.Shell', '/org/gnome/Shell', 'org.freedesktop.DBus.Properties',
                       'Get', GLib.Variant('(ss)', ('org.gnome.Shell', 'ShellVersion'))).unpack()[0]
        if not str(version).startswith('51.'):
            raise ValueError('unsupported Mutter version')
        path = dbus(bus, dest, '/org/gnome/Mutter/RemoteDesktop', dest, 'CreateSession').unpack()[0]
        xml = dbus(bus, dest, path, 'org.freedesktop.DBus.Introspectable', 'Introspect').unpack()[0]
        for method in ['EnableClipboard', 'SetSelection', 'SelectionRead', 'SelectionWrite', 'SelectionWriteDone']:
            if f'name="{method}"' not in xml:
                raise ValueError('Mutter clipboard capability missing')
        if not allowed():
            return
        bus.signal_subscribe(dest, interface, None, path, None, Gio.DBusSignalFlags.NONE, on_signal)
        call('EnableClipboard', GLib.Variant('(a{sv})', ({},)))
        call('Start')
        backend = 'mutter-51'
    except GLib.Error:
        path = None
        # wl-clipboard watch is only accepted on a compositor supporting data
        # control; GNOME never falls back to polling/stealing X11 selections.
        backend = 'hyprland-data-control'
        # Resolve the live compositor environment from its process, same UID.
        os.environ.update(hypr_environment())
        watcher = subprocess.Popen(['wl-paste', '--type', 'text', '--watch', HELPER, '--selection'], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        def watch():
            try:
                for value in frames(watcher.stdout):
                    outgoing.put(value['text'].encode('utf-8'), timeout=1)
            except (EOFError, OSError, ValueError, queue.Full):
                pass
        threading.Thread(target=watch, daemon=True).start()

    def reader():
        try:
            for value in frames(sys.stdin.buffer):
                incoming.put(value, timeout=1)
        except (EOFError, OSError, ValueError, queue.Full):
            pass
        finally:
            ending.set()
    threading.Thread(target=reader, daemon=True).start()

    def tick():
        nonlocal enabled, content, host_sequence, selection_epoch, copy_owner
        try:
            if ending.is_set() or not allowed():
                enabled = False
                content = b''
                loop.quit()
                return False
            while not outgoing.empty():
                send_text(outgoing.get_nowait())
            while not incoming.empty():
                value = incoming.get_nowait()
                if value.get('schema') != 1 or value.get('generation') != generation or value.get('origin') != 'host' or value.get('op') != 'set':
                    continue
                sequence = value.get('sequence')
                if not isinstance(sequence, int) or not host_sequence < sequence < 2**63:
                    continue
                host_sequence = sequence
                selection_epoch += 1
                content = value['text'].encode('utf-8')
                if path is not None:
                    call('SetSelection', GLib.Variant('(a{sv})', ({'mime-types': GLib.Variant('as', ['text/plain;charset=utf-8', 'text/plain'])},)))
                else:
                    stop_helper(copy_owner)
                    # Keep the Wayland selection owner in this session's
                    # process group, so lock/EOF cannot leave a daemon behind.
                    copy_owner = subprocess.Popen(['wl-copy', '--foreground', '--type', 'text/plain;charset=utf-8'], stdin=subprocess.PIPE, stderr=subprocess.DEVNULL)
                    pipe_io(os.dup(copy_owner.stdin.fileno()), content)
                    copy_owner.stdin.close()
            return True
        except (GLib.Error, OSError, ValueError, subprocess.SubprocessError):
            loop.quit()
            return False
    GLib.timeout_add(100, tick)
    emit({'op': 'ready', 'schema': 1, 'generation': generation, 'backend': backend})
    try:
        loop.run()
    finally:
        enabled = False
        content = b''
        if path is not None:
            try:
                call('Stop')
            except GLib.Error:
                pass
        stop_helper(watcher)
        stop_helper(copy_owner)
        emit({'op': 'inactive', 'schema': 1})


if __name__ == '__main__':
    try:
        if sys.argv[1:] == ['--supervise'] and os.getuid() == 0:
            supervise()
        elif sys.argv[1:] == ['--session'] and os.getuid() != 0:
            session()
        elif sys.argv[1:] == ['--selection'] and os.getuid() != 0:
            data = sys.stdin.buffer.read(MAX_TEXT + 1)
            if len(data) <= MAX_TEXT:
                emit({'text': data.decode('utf-8')})
        elif sys.argv[1:] == ['--display-info']:
            display_info()
        elif len(sys.argv) == 4 and sys.argv[1] == '--resize':
            target = tuple(int(n) for n in sys.argv[2:])
            if not all(1 <= n <= 16384 for n in target):
                raise ValueError('display size limit')
            display_info(target)
    except (EOFError, OSError, ValueError, GLib.Error, subprocess.SubprocessError):
        # Never put exception text (possibly containing clipboard data) in logs.
        raise SystemExit(1)
