"""Observe real close-window/Ctrl-C/forced-launcher-exit teardown.

Start before the Cua action. Forced exit terminates only the exact validated
launcher that owns this vm.json. Raw results distinguish normal cleanup from
crash leftovers, which the next launch of that purpose must reclaim.
"""
import argparse
import ctypes as C
from ctypes import wintypes as W
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--info', type=Path, required=True)
p.add_argument('--owner', required=True)
p.add_argument('--launcher', type=Path, required=True)
p.add_argument('--vmctl', type=Path, required=True)
p.add_argument('--out', type=Path, required=True)
p.add_argument('mode', choices=['close-window', 'ctrl-c', 'forced-exit'])
a = p.parse_args()
i = json.loads(a.info.read_text())
assert i['schema'] == 1 and i['owner'] == a.owner and i['qmp_endpoint'].startswith('unix:')
a.out.mkdir(parents=True, exist_ok=True)
k = C.WinDLL('kernel32', use_last_error=True)
k.OpenProcess.argtypes = [W.DWORD, W.BOOL, W.DWORD]
k.OpenProcess.restype = W.HANDLE
k.CloseHandle.argtypes = [W.HANDLE]
k.GetProcessTimes.argtypes = [W.HANDLE, C.POINTER(W.FILETIME), C.POINTER(W.FILETIME), C.POINTER(W.FILETIME), C.POINTER(W.FILETIME)]
k.QueryFullProcessImageNameW.argtypes = [W.HANDLE, W.DWORD, W.LPWSTR, C.POINTER(W.DWORD)]
k.WaitForSingleObject.argtypes = [W.HANDLE, W.DWORD]
k.TerminateProcess.argtypes = [W.HANDLE, W.UINT]
k.GetExitCodeProcess.argtypes = [W.HANDLE, C.POINTER(W.DWORD)]


def identity(handle):
    times = [W.FILETIME() for _ in range(4)]
    assert k.GetProcessTimes(handle, *(C.byref(t) for t in times))
    path, size = C.create_unicode_buffer(32768), W.DWORD(32768)
    assert k.QueryFullProcessImageNameW(handle, 0, path, C.byref(size))
    return {'process_start': str(times[0].dwLowDateTime | times[0].dwHighDateTime << 32),
            'executable': path.value}


launcher_pid = int(i['run'].split('-')[0])
launcher = k.OpenProcess(0x100000 | 0x1000 | (1 if a.mode == 'forced-exit' else 0), False, launcher_pid)
qemu = k.OpenProcess(0x100000 | 0x1000, False, i['pid'])
assert launcher and qemu
try:
    launcher_identity, qemu_identity = identity(launcher), identity(qemu)
    assert Path(launcher_identity['executable']).resolve() == a.launcher.resolve()
    assert qemu_identity['process_start'] == i['process_start']
    assert k.WaitForSingleObject(launcher, 0) == 258 and k.WaitForSingleObject(qemu, 0) == 258
    run_dir = a.info.parent / i['run']
    assert Path(i['ssh_key']) == run_dir / 'id_ed25519'
    paths = [run_dir / name for name in ['id_ed25519', 'id_ed25519.pub', 'host_ed25519', 'host_ed25519.pub']]
    sockets = [Path(i[key][5:]) for key in ['qmp_endpoint', 'input_endpoint']]
    assert all(path.exists() for path in paths + sockets)
    result = {'schema': 1, 'mode': a.mode, 'run': i['run'], 'owner': i['owner'],
              'started_utc': datetime.now(timezone.utc).isoformat(),
              'launcher_pid': launcher_pid, 'launcher': launcher_identity,
              'qemu_pid': i['pid'], 'qemu': qemu_identity,
              'qemu_hwnd': i['hwnd'], 'keys_present_before': True, 'sockets_present_before': True}
    (a.out / 'ready.json').write_text(json.dumps(result, indent=2))
    started = time.monotonic()
    if a.mode == 'forced-exit':
        assert k.TerminateProcess(launcher, 37), C.get_last_error()
    assert k.WaitForSingleObject(qemu, 90000) == 0, 'QEMU outlived teardown deadline'
    assert k.WaitForSingleObject(launcher, 20000) == 0, 'Launcher outlived teardown deadline'
    result.update({'observed_exit_utc': datetime.now(timezone.utc).isoformat(),
                   'action_wait_and_teardown_seconds': time.monotonic() - started,
                   'qemu_exited': True, 'launcher_exited': True,
                   'keys_remaining': [p.name for p in paths if p.exists()],
                   'sockets_remaining': [p.name for p in sockets if p.exists()],
                   'ephemeral_disk_remaining': (run_dir / 'overlay.qcow2').exists(),
                   'ephemeral_vars_remaining': (run_dir / 'overlay.vars.fd').exists()})
    for name, handle in [('launcher', launcher), ('qemu', qemu)]:
        code = W.DWORD()
        assert k.GetExitCodeProcess(handle, C.byref(code))
        result[name + '_exit_code'] = code.value
    teardown = a.info.parent / 'teardown-status.json'
    if teardown.exists():
        value = json.loads(teardown.read_text())
        if value.get('run') == i['run']:
            result['launcher_teardown'] = value
    stale = subprocess.run([str(a.vmctl), '--vm', i['purpose'], 'self-check'], capture_output=True, timeout=10)
    result['stale_attach_exit'] = stale.returncode
    result['stale_attach_error'] = stale.stderr.decode(errors='replace')
    assert stale.returncode != 0, 'vmctl accepted a dead VM'
    (a.out / 'result.json').write_text(json.dumps(result, indent=2))
    if a.mode != 'forced-exit':
        assert result['launcher_exit_code'] == 0 and result['qemu_exit_code'] == 0
        assert not result['keys_remaining'] and not result['sockets_remaining']
        assert not result['ephemeral_disk_remaining'] and not result['ephemeral_vars_remaining']
    print(json.dumps(result))
finally:
    k.CloseHandle(qemu)
    k.CloseHandle(launcher)
