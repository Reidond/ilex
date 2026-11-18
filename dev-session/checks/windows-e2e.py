"""Observable Windows/guest checks for a running owned development VM.

Uses the installed spike venv (paramiko/Pillow), or an equivalent private venv.
Cua must select/focus the exact window before `input`; foreground input never
uses posted window messages. No unit tests or fabricated guest events.
"""
import argparse
import ctypes
from ctypes import wintypes as W
import json
from pathlib import Path
import queue
import subprocess
import threading
import time
import sys
import paramiko

sys.dont_write_bytecode = True

parser = argparse.ArgumentParser()
parser.add_argument('--info', required=True, type=Path)
parser.add_argument('--artifacts', required=True, type=Path)
parser.add_argument('--vmctl', required=True, type=Path)
parser.add_argument('--motion-seconds', type=float, default=65)
parser.add_argument('--limits', action='store_true', help='Also exercise real clipboard UTF-8 byte limits')
parser.add_argument('--previous', type=Path, help='Earlier ssh-identity.json from the same persistent overlay')
parser.add_argument('operation', choices=['provision', 'clipboard', 'relogin', 'lock', 'qt-clipboard', 'input', 'focus', 'hold-shift', 'release-shift', 'facts', 'ssh-identity'])
args = parser.parse_args()
info = json.loads(args.info.read_text())
args.artifacts.mkdir(parents=True, exist_ok=True)
assert info['schema'] == 1 and info['display'] == 'sdl-gl'
user32 = ctypes.WinDLL('user32', use_last_error=True)
user32.GetForegroundWindow.restype = W.HWND
user32.GetWindowTextW.argtypes = [W.HWND, W.LPWSTR, ctypes.c_int]
user32.GetWindowThreadProcessId.argtypes = [W.HWND, ctypes.POINTER(W.DWORD)]
user32.IsWindow.argtypes = [W.HWND]


def validate_window(foreground=False):
    owner = W.DWORD()
    assert user32.IsWindow(info['hwnd'])
    user32.GetWindowThreadProcessId(info['hwnd'], ctypes.byref(owner))
    assert owner.value == info['pid']
    if foreground:
        assert user32.GetForegroundWindow() == info['hwnd'], 'Refusing SendInput: QEMU is not foreground'


validate_window()
client = paramiko.SSHClient()
hostkeys = paramiko.HostKeys(str(info['known_hosts']))
expected = hostkeys.lookup(info['host_key_alias'])
assert expected is not None
for kind, key in expected.items():
    client.get_host_keys().add(f"[127.0.0.1]:{info['ssh_port']}", kind, key)
client.set_missing_host_key_policy(paramiko.RejectPolicy())
client.connect('127.0.0.1', port=info['ssh_port'], username='root', key_filename=info['ssh_key'],
               look_for_keys=False, allow_agent=False, timeout=5)


def ssh(command, timeout=30):
    stream, output, error = client.exec_command(command, timeout=timeout)
    stream.close()
    data = output.read()
    diagnostics = error.read()
    assert output.channel.recv_exit_status() == 0, diagnostics.decode(errors='replace')
    return data


class Keyboard(ctypes.Structure):
    _fields_ = [('vk', W.WORD), ('scan', W.WORD), ('flags', W.DWORD), ('time', W.DWORD), ('extra', ctypes.c_size_t)]


class InputUnion(ctypes.Union):
    _fields_ = [('keyboard', Keyboard), ('padding', ctypes.c_byte * 32)]


class Input(ctypes.Structure):
    _fields_ = [('type', W.DWORD), ('data', InputUnion)]


def key(vk, up=False):
    validate_window(foreground=not up)
    item = Input()
    item.type = 1
    item.data.keyboard = Keyboard(0, user32.MapVirtualKeyW(vk, 0), 8 | (2 if up else 0) | (1 if vk in [0xa3, 0xa5, 0x5b, 0x5c] else 0), 0, 0)
    assert user32.SendInput(1, ctypes.byref(item), ctypes.sizeof(item)) == 1
    time.sleep(.08)


def chord(*keys):
    held = []
    try:
        for code in keys:
            key(code)
            held.append(code)
    finally:
        for code in reversed(held):
            key(code, up=True)


def caption():
    text = ctypes.create_unicode_buffer(1024)
    user32.GetWindowTextW(info['hwnd'], text, len(text))
    return text.value


def ctl(*command):
    return subprocess.check_output([str(args.vmctl), '--vm', info['purpose'], *command], cwd=Path(__file__).resolve().parents[2])


if args.operation == 'provision':
    with client.open_sftp() as sftp:
        for name in ['gtk-motion.py', 'evdev.py', 'qt-motion.qml']:
            sftp.put(str(Path(__file__).with_name(name)), f'/var/tmp/ilex-t4-{name}')
    ssh("id ilexdev >/dev/null 2>&1 || useradd -m -s /bin/bash ilexdev; passwd -d ilexdev; install -d -o ilexdev -g ilexdev /var/home/ilexdev/.config; printf 'yes\\n' >/var/home/ilexdev/.config/gnome-initial-setup-done; chown ilexdev:ilexdev /var/home/ilexdev/.config/gnome-initial-setup-done; printf '[daemon]\\nAutomaticLoginEnable=True\\nAutomaticLogin=ilexdev\\n' >/etc/gdm/custom.conf; systemctl disable --now bootc-fetch-apply-updates.timer; systemctl restart gdm")
    print('Disposable guest user and GDM autologin provisioned')
elif args.operation == 'facts':
    data = ssh("uname -r; rpm -q gnome-shell mutter mesa-dri-drivers; bootc status --json; journalctl -b --no-pager -g 'GBM renderer|virtio_gpu' | tail -30")
    (args.artifacts / 'guest-facts.txt').write_bytes(data)
    print(data.decode())
elif args.operation == 'ssh-identity':
    import hashlib
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
    public = ssh('cat /run/ilex-dev/authorized_keys/root').split()[:2]
    assert public == Path(info['ssh_key']).with_suffix('.pub').read_bytes().split()[:2]
    (args.artifacts / 'ssh-provisioning.txt').write_bytes(ssh('getenforce; ls -ldZ /run/ilex-dev/authorized_keys /run/ilex-dev/authorized_keys/root'))
    random_public = Ed25519PrivateKey.generate().public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    wrong = paramiko.Ed25519Key(data=b'\x00\x00\x00\x0bssh-ed25519\x00\x00\x00\x20' + random_public)
    negative = paramiko.SSHClient()
    negative.get_host_keys().add(f"[127.0.0.1]:{info['ssh_port']}", 'ssh-ed25519', wrong)
    negative.set_missing_host_key_policy(paramiko.RejectPolicy())
    rejected = False
    try:
        negative.connect('127.0.0.1', port=info['ssh_port'], username='root', key_filename=info['ssh_key'],
                         look_for_keys=False, allow_agent=False, timeout=5)
    except paramiko.BadHostKeyException:
        rejected = True
    finally:
        negative.close()
    assert rejected
    result = {'run': info['run'], 'disk': Path(info['disk']).name, 'correct_pin_accepted': True,
              'wrong_pin_rejected': True, 'firmware_access_key_matches': True,
              'host_pin_sha256': hashlib.sha256(client.get_transport().get_remote_server_key().asbytes()).hexdigest(),
              'access_key_sha256': hashlib.sha256(public[1]).hexdigest()}
    if args.previous:
        previous = json.loads(args.previous.read_text())
        assert previous['disk'] == result['disk'] and previous['run'] != result['run']
        assert all(previous[k] != result[k] for k in ['host_pin_sha256', 'access_key_sha256'])
        result['persistent_overlay_new_keys_passed'] = True
    (args.artifacts / 'ssh-identity.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result))
elif args.operation in ['hold-shift', 'release-shift']:
    key(0xa0, args.operation == 'release-shift')
    print(args.operation)
elif args.operation == 'input':
    validate_window(foreground=True)
    hyprland = json.loads(ctl('display'))['backend'] == 'hyprland'
    if hyprland:
        uid = int(ssh('id -u ilexdev').decode().strip())
        marker = f'/run/user/{uid}/ilex-t4-super-e'
        ssh(f'rm -f {marker}')
    source = client.exec_command('python3 -u /var/tmp/ilex-t4-evdev.py 14')[1]
    assert json.loads(source.readline())['ready']
    if 'exit grab' not in caption():
        chord(0xa3, 0x47)
    assert 'exit grab' in caption(), 'SDL did not grab'
    titles = []
    for name, codes in [('super', [0x5b]), ('super-e', [0x5b, 0x45]), ('alt-tab', [0x12, 0x09]), ('ctrl-esc', [0x11, 0x1b])]:
        for _ in range(3):
            chord(*codes)
        assert user32.GetForegroundWindow() == info['hwnd'] and 'exit grab' in caption(), 'SDL grab escaped unexpectedly'
        titles.append({'chord': name, 'caption': caption(), 'foreground_qemu': True})
    chord(0xa3, 0x47)
    assert 'exit grab' not in caption(), 'Right-Ctrl+G did not escape'
    events = [json.loads(line) for line in source]
    (args.artifacts / 'guest-input.json').write_text(json.dumps(events, indent=2))
    for code in [125, 18, 56, 15, 29, 1]:
        assert any(e.get('code') == code and e.get('value') == 1 for e in events), ('missing down', code)
        assert any(e.get('code') == code and e.get('value') == 0 for e in events), ('missing up', code)
    held = set()
    for event in events:
        if event.get('code') in [125, 18, 56, 15, 29, 1, 97, 34]:
            item = (event['device'], event['code'])
            if event['value'] == 1:
                held.add(item)
            elif event['value'] == 0:
                held.discard(item)
    assert not held, ('guest keys left held after escape', sorted(held))
    result = {'route': 'SendInput physical set-1 scan codes with extended flags', 'chords': titles, 'right_ctrl_g_escape': True, 'no_held_test_keys': True}
    if hyprland:
        dispatched = int(ssh(f'wc -l < {marker}').decode().strip())
        assert dispatched == 3, ('Hyprland Super+E action count', dispatched)
        result['hyprland_super_e_actions'] = dispatched
    (args.artifacts / 'host-input.json').write_text(json.dumps(result, indent=2))
    print('SDL real-input matrix passed')
elif args.operation == 'focus':
    validate_window(foreground=True)
    source = client.exec_command('python3 -u /var/tmp/ilex-t4-evdev.py 14')[1]
    assert json.loads(source.readline())['ready']
    events = []
    def event_reader():
        for line in source:
            events.append({'observed': time.monotonic(), **json.loads(line)})
    threading.Thread(target=event_reader, daemon=True).start()
    if 'exit grab' not in caption():
        chord(0xa3, 0x47)
    assert 'exit grab' in caption()
    key(0xa0)
    try:
        until = time.monotonic() + 2
        while time.monotonic() < until and not any(e['code'] == 42 and e['value'] == 1 for e in events):
            time.sleep(.05)
        assert any(e['code'] == 42 and e['value'] == 1 for e in events)
        (args.artifacts / 'focus-ready.json').write_text(json.dumps({'shift_down_in_guest': True}))
        until = time.monotonic() + 10
        while time.monotonic() < until:
            if user32.GetForegroundWindow() != info['hwnd'] and any(e['code'] == 42 and e['value'] == 0 for e in events):
                break
            time.sleep(.05)
        released = any(e['code'] == 42 and e['value'] == 0 for e in events)
        host_still_held = bool(user32.GetAsyncKeyState(0xa0) & 0x8000)
        result = {'guest_release_before_host_release': released, 'host_shift_still_held': host_still_held, 'focus_left_qemu': user32.GetForegroundWindow() != info['hwnd']}
    finally:
        key(0xa0, up=True)
    result['host_shift_released'] = not bool(user32.GetAsyncKeyState(0xa0) & 0x8000)
    (args.artifacts / 'focus-loss.json').write_text(json.dumps(result, indent=2))
    (args.artifacts / 'focus-events.json').write_text(json.dumps(events, indent=2))
    assert all(result.values()), result
    print('SDL focus-loss release passed')
elif args.operation == 'qt-clipboard':
    # Real Qt Quick TextInputs consume/offer the Wayland selection. Paste is
    # masked; the child emits booleans and frame statistics, never pasted text.
    import importlib.util
    spec = importlib.util.spec_from_file_location('clipboard', Path(__file__).with_name('win-clipboard.py'))
    clipboard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(clipboard)
    assert json.loads(ctl('display'))['backend'] == 'hyprland'
    uid = int(ssh('id -u ilexdev').decode().strip())
    launch = f'runuser -u ilexdev -- env XDG_RUNTIME_DIR=/run/user/{uid} DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus systemd-run --user --quiet --pipe --wait --collect --service-type=exec --property=RuntimeMaxSec=120 env QT_QPA_PLATFORM=wayland QSG_RHI_BACKEND=opengl QSG_INFO=1 QT_LOGGING_RULES=qt.rhi.general=true qml-qt6 /var/tmp/ilex-t4-qt-motion.qml'
    previous = clipboard.read()
    stream, output, error = client.exec_command(launch)
    stream.close()
    messages = queue.Queue()
    diagnostics = []
    def qt_reader():
        for line in error:
            diagnostics.append(line.rstrip())
            at = line.find('{')
            if at >= 0:
                try:
                    messages.put(json.loads(line[at:]))
                except json.JSONDecodeError:
                    pass
    threading.Thread(target=qt_reader, daemon=True).start()
    status_path = args.info.with_name('session-status.json')
    try:
        until = time.monotonic() + 20
        while time.monotonic() < until:
            if messages.get(timeout=20).get('qt_ready'):
                break
        else:
            raise AssertionError('Qt window did not become active')
        validate_window(foreground=True)
        chord(0x11, 0x41)
        chord(0x11, 0x43)
        until = time.monotonic() + 5
        guest_text = 'ilex T4 Qt guest → host Привіт 世界 🚀 ✓'
        while time.monotonic() < until and clipboard.read() != guest_text:
            time.sleep(.1)
        reverse = clipboard.read() == guest_text
        clipboard.write('ilex T4 Qt host → guest Україна 日本語 🙂 ✓')
        time.sleep(2)
        chord(0x09)
        chord(0x11, 0x56)
        until = time.monotonic() + 5
        forward = False
        while time.monotonic() < until:
            value = messages.get(timeout=5)
            if value.get('qt_host_to_guest'):
                forward = True
                break
        result = {'qt_guest_to_host': reverse, 'qt_host_to_guest': forward, 'route': 'SendInput physical Ctrl+A/C, Tab, Ctrl+V into Qt Quick TextInputs'}
        (args.artifacts / 'qt-clipboard.json').write_text(json.dumps(result, indent=2))
        print(json.dumps(result))
        assert reverse and forward
        time.sleep(2)
        idle_start = json.loads(status_path.read_text())
        time.sleep(min(args.motion_seconds, 65))
        idle_end = json.loads(status_path.read_text())
        assert all(idle_start[k] == idle_end[k] for k in ['host_updates', 'guest_updates']), 'Qt clipboard feedback loop'
        (args.artifacts / 'qt-clipboard-idle.json').write_text(json.dumps({'before': idle_start, 'after': idle_end}, indent=2))
        assert output.channel.recv_exit_status() == 0
        (args.artifacts / 'qt-renderer-and-motion.txt').write_text('\n'.join(diagnostics))
        assert any('virgl' in line.lower() for line in diagnostics)
        metrics = []
        while not messages.empty():
            value = messages.get_nowait()
            if 'samples' in value:
                metrics.append(value)
        assert metrics and metrics[-1]['samples'] > 0
        (args.artifacts / 'qt-motion.json').write_text(json.dumps(metrics, indent=2))
    finally:
        (args.artifacts / 'qt-renderer-and-motion.txt').write_text('\n'.join(diagnostics))
        if previous is not None:
            clipboard.write(previous)
elif args.operation in ['clipboard', 'relogin', 'lock']:
    # Independent Wayland client. Transport log contains only renderer/fps and
    # boolean assertions; clipboard payloads and previous host text stay in RAM.
    import importlib.util
    source = Path(__file__).with_name('win-clipboard.py')
    spec = importlib.util.spec_from_file_location('clipboard', source)
    clipboard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(clipboard)
    previous = clipboard.read()
    status_path = args.info.with_name('session-status.json')
    def status():
        return json.loads(status_path.read_text())
    if args.operation == 'lock':
        before = status()
        assert before['active']
        sessions = [line.split() for line in ssh('loginctl list-sessions --no-legend').decode().splitlines()]
        session = [row[0] for row in sessions if len(row) > 3 and row[2:4] == ['ilexdev', 'seat0']]
        assert len(session) == 1 and session[0].isalnum()
        try:
            ssh('loginctl lock-session ' + session[0])
            until = time.monotonic() + 15
            while time.monotonic() < until and status()['active']:
                time.sleep(.1)
            assert not status()['active']
            locked = status()
            clipboard.write('ilex T4 locked-session privacy probe Україна 日本語 🙂')
            time.sleep(3)
            after = status()
            assert not after['active']
            assert all(locked[k] == after[k] for k in ['host_updates', 'guest_updates'])
        finally:
            ssh('loginctl unlock-session ' + session[0])
        until = time.monotonic() + 20
        while time.monotonic() < until and not status()['active']:
            time.sleep(.1)
        assert status()['active'] and status()['activations'] > before['activations']
        (args.artifacts / 'lock.json').write_text(json.dumps({'inactive_while_locked': True, 'no_transfers_while_locked': True, 'automatically_reactivated': True}, indent=2))
    if args.operation == 'relogin':
        before = status()
        assert before['active']
        gdm_before = ssh('systemctl show gdm.service --property=InvocationID --value').decode().strip()
        ssh('systemctl restart gdm')
        until = time.monotonic() + 90
        inactive = False
        while time.monotonic() < until:
            current = status()
            inactive |= not current['active'] or current.get('deactivations', 0) > before.get('deactivations', 0)
            if inactive and current['active'] and current.get('activations', 0) > before.get('activations', 0):
                break
            time.sleep(.2)
        else:
            raise AssertionError('clipboard did not automatically reconnect after guest login')
        gdm_after = ssh('systemctl show gdm.service --property=InvocationID --value').decode().strip()
        assert gdm_before and gdm_before != gdm_after
        (args.artifacts / 'relogin.json').write_text(json.dumps({'inactive_observed': inactive, 'reactivated': True, 'gdm_before': gdm_before, 'gdm_after': gdm_after}, indent=2))
    uid = int(ssh('id -u ilexdev').decode().strip())
    launch = f'runuser -u ilexdev -- env XDG_RUNTIME_DIR=/run/user/{uid} DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus systemd-run --user --quiet --pipe --wait --collect --service-type=exec --property=RuntimeMaxSec=180 python3 -u /var/tmp/ilex-t4-gtk-motion.py'
    stdin, stdout, stderr = client.exec_command(launch)
    messages = queue.Queue()
    render = []
    def reader():
        for line in stdout:
            value = json.loads(line)
            if 'text' not in value:
                render.append(value)
            messages.put(value)
    threading.Thread(target=reader, daemon=True).start()
    def send(value):
        stdin.write(json.dumps(value) + '\n')
        stdin.flush()
    def read_text():
        send({'op': 'get'})
        until = time.monotonic() + 5
        while time.monotonic() < until:
            value = messages.get(timeout=5)
            if 'text' in value:
                return value['text']
        raise AssertionError('clipboard client timeout')
    try:
        time.sleep(3)
        ctl('key', 'Escape')
        text = 'ilex T4 host → guest Україна 日本語 🙂 ✓'
        clipboard.write(text)
        time.sleep(2)
        first = read_text() == text
        second = read_text() == text
        other = 'ilex T4 guest → host Привіт 世界 🚀 ✓'
        send({'op': 'set', 'text': other})
        until = time.monotonic() + 5
        while time.monotonic() < until and clipboard.read() != other:
            time.sleep(.1)
        reverse = clipboard.read() == other
        result = {'host_to_guest_first': first, 'host_to_guest_second': second, 'guest_to_host': reverse}
        (args.artifacts / 'clipboard.json').write_text(json.dumps(result, indent=2))
        print(json.dumps(result))
        assert all(result.values())
        if args.limits:
            ascii_maximum = 'a' * (256 * 1024)
            clipboard.write(ascii_maximum)
            time.sleep(2)
            ascii_maximum_passed = read_text() == ascii_maximum
            maximum = '🙂' * (256 * 1024 // 4)
            oversize = maximum + '🙂'
            clipboard.write(maximum)
            time.sleep(2)
            maximum_passed = read_text() == maximum
            clipboard.write(oversize)
            time.sleep(2)
            host_oversize_refused = read_text() == maximum
            baseline = 'ilex T4 bounded clipboard recovery ✓'
            clipboard.write(baseline)
            time.sleep(2)
            assert read_text() == baseline
            send({'op': 'set', 'text': oversize})
            time.sleep(2)
            guest_oversize_refused = clipboard.read() == baseline
            send({'op': 'set', 'text': other})
            until = time.monotonic() + 5
            while time.monotonic() < until and clipboard.read() != other:
                time.sleep(.1)
            limits = {'maximum_utf8_bytes': len(maximum.encode()), 'maximum_ascii_host_to_guest': ascii_maximum_passed, 'maximum_host_to_guest': maximum_passed,
                      'oversize_host_to_guest_refused': host_oversize_refused, 'oversize_guest_to_host_refused': guest_oversize_refused,
                      'normal_transfer_recovers': clipboard.read() == other}
            (args.artifacts / 'byte-limits.json').write_text(json.dumps(limits, indent=2))
            assert all(limits.values()), limits
        # Keep the renderer alive for a sustained sample. Cua can resize and
        # native vmctl can capture while this independent workload is running.
        time.sleep(2)
        idle_start = status()
        time.sleep(args.motion_seconds)
        idle_end = status()
        for counter in ['host_updates', 'guest_updates']:
            assert idle_end[counter] == idle_start[counter], ('clipboard feedback loop', counter)
        (args.artifacts / 'clipboard-idle-counters.json').write_text(json.dumps({'before': idle_start, 'after': idle_end}, indent=2))
        (args.artifacts / 'gnome-motion.json').write_text(json.dumps(render, indent=2))
        assert any('virgl' in item.get('renderer', '').lower() for item in render)
    finally:
        if previous is not None:
            clipboard.write(previous)
        send({'op': 'quit'})
        stdin.close()
client.close()
