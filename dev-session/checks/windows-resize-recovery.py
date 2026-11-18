"""Real guest login/compositor regression, without a second host resize.

For delayed-login, resize the Cua-selected QEMU window after ready.json, then
create continue. The guest stays logged out for another 25 seconds. For a
session change, configure the next GDM session first, then use session-change.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
from datetime import datetime, timezone
import json
from pathlib import Path
import time
import paramiko

p = argparse.ArgumentParser()
p.add_argument('--info', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
p.add_argument('--backend', choices=['mutter', 'hyprland'], required=True)
p.add_argument('--expect-guest-mode-recovery', action='store_true')
p.add_argument('stage', choices=['delayed-login', 'session-change'])
a = p.parse_args()
i = json.loads(a.info.read_text())
assert i['schema'] == 1 and i['display'] == 'sdl-gl'
a.out.mkdir(parents=True, exist_ok=True)
u = C.WinDLL('user32', use_last_error=True)
u.SetThreadDpiAwarenessContext.argtypes = [C.c_void_p]
u.SetThreadDpiAwarenessContext.restype = C.c_void_p
u.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
u.GetClientRect.argtypes = [W.HWND, C.POINTER(W.RECT)]
u.IsIconic.argtypes = [W.HWND]


def client_size():
    pid = W.DWORD()
    assert u.GetWindowThreadProcessId(i['hwnd'], C.byref(pid)) and pid.value == i['pid']
    assert not u.IsIconic(i['hwnd']), 'Regression window was minimized'
    previous = u.SetThreadDpiAwarenessContext(C.c_void_p(-4))
    assert previous
    try:
        rect = W.RECT()
        assert u.GetClientRect(i['hwnd'], C.byref(rect))
        return [rect.right - rect.left, rect.bottom - rect.top]
    finally:
        u.SetThreadDpiAwarenessContext(previous)


c = paramiko.SSHClient()
for kind, key in paramiko.HostKeys(i['known_hosts']).lookup(i['host_key_alias']).items():
    c.get_host_keys().add(f"[127.0.0.1]:{i['ssh_port']}", kind, key)
c.set_missing_host_key_policy(paramiko.RejectPolicy())
c.connect('127.0.0.1', port=i['ssh_port'], username='root', key_filename=i['ssh_key'],
          look_for_keys=False, allow_agent=False, timeout=5)


def utc():
    return datetime.now(timezone.utc).isoformat()


def command(text):
    stdin, stdout, stderr = c.exec_command(text, timeout=30)
    stdin.close()
    data, error = stdout.read(), stderr.read()
    assert stdout.channel.recv_exit_status() == 0, error.decode(errors='replace')
    return data.decode()


rows = []


def sample():
    value = json.loads((a.info.parent / 'resize-status.json').read_text())
    assert value['run'] == i['run'], 'Resize status belongs to another launch'
    rows.append({'utc': utc(), 'host_client_size': client_size(), 'resize': value})
    with (a.out / 'timeline.jsonl').open('a') as f:
        f.write(json.dumps(rows[-1]) + '\n')
    return value


try:
    baseline = sample()
    if a.stage == 'delayed-login':
        command('systemctl stop gdm')
        (a.out / 'ready.json').write_text(json.dumps({'utc': utc(), 'state': 'gdm-stopped', 'run': i['run']}))
        until = time.monotonic() + 90
        while not (a.out / 'continue').exists():
            sample()
            assert time.monotonic() < until, 'Cua resize acknowledgement timed out'
            time.sleep(1)
        # Exceeds the old twelve-second retry window with no display session.
        until = time.monotonic() + 25
        while time.monotonic() < until:
            sample()
            time.sleep(1)
        unavailable = [row['resize'] for row in rows if not row['resize'].get('acknowledged')]
        assert len(unavailable) >= 3
        assert unavailable[-1]['observed_unix_ms'] - unavailable[0]['observed_unix_ms'] > 12000
        assert unavailable[-1]['attempt'] > unavailable[0]['attempt']
        command('systemctl start gdm')
    else:
        assert baseline['acknowledged'], 'Need a previously acknowledged display'
        assert client_size() == baseline['target'], 'Baseline differs from physical host client'
        command('systemctl restart gdm')
    restarted = utc()
    recovered = None
    until = time.monotonic() + 90
    while time.monotonic() < until:
        value = sample()
        if (value.get('acknowledged') and value.get('backend') == a.backend
                and value.get('session') != baseline.get('session')
                and (a.stage != 'session-change' or value.get('target') == baseline.get('target'))
                and value.get('attempt', 0) > baseline.get('attempt', 0)):
            recovered = value
            break
        time.sleep(1)
    assert recovered is not None, 'Display did not recover after login/session change'
    assert client_size() == recovered['target'], 'Recovered guest mode differs from physical host client'
    # Continued observations establish that recovery persists at the same size.
    for _ in range(5):
        time.sleep(1)
        value = sample()
        assert value['acknowledged'] and value['target'] == recovered['target']
        assert client_size() == recovered['target'], 'Physical host client changed after recovery'
    if a.stage == 'session-change':
        assert recovered['target'] == baseline['target'], 'Host window changed during session regression'
    if a.expect_guest_mode_recovery:
        assert recovered['guest_mode_recoveries'] > baseline.get('guest_mode_recoveries', 0)
    result = {'schema': 1, 'stage': a.stage, 'run': i['run'], 'utc': utc(),
              'gdm_started_utc': restarted, 'baseline': baseline, 'recovered': recovered,
              'physical_host_client_size': client_size(),
              'no_second_host_resize': True, 'continued_acknowledgements': True}
    (a.out / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
finally:
    c.close()
